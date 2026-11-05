//! Shows two processes holding different data at the same virtual address.
//!
//! A static and an `mmap`ed page have the same address in every process running this.
//! The parent starts a child, which writes its own values at the same addresses and
//! waits to be overlapped; each then checks what it reads is still its own.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU64, Ordering};

use feos_user::{abi, entry, getpid, mmap, parse_number, println, sleep_ms, spawn, wait, Args};

static VALUE: AtomicU64 = AtomicU64::new(0);

/// Writes `value` to both places, holds it a while, and says whether it's still there.
fn hold(role: &str, value: u64, ms: usize) -> bool {
    let page = mmap(abi::PAGE_SIZE, abi::prot::READ | abi::prot::WRITE);
    if page <= 0 {
        println!("vmcheck {role}: mmap failed");
        return false;
    }
    let cell = page as usize as *mut u64;
    VALUE.store(value, Ordering::SeqCst);
    // SAFETY: the page was mapped just now, for this.
    unsafe { cell.write_volatile(value) };
    let mut good = true;
    let rounds = ms / 10;
    for _ in 0..rounds {
        sleep_ms(10);
        // SAFETY: as above.
        good &= VALUE.load(Ordering::SeqCst) == value && unsafe { cell.read_volatile() } == value;
    }
    println!(
        "vmcheck {role} pid={} static={:#x} mmap={:#x} value={value} intact={good}",
        getpid(),
        &VALUE as *const AtomicU64 as usize,
        cell as usize,
    );
    good
}

fn main(args: Args) -> i32 {
    if let Some(value) = args.value_of("--child").and_then(parse_number) {
        return if hold("child", value as u64, 400) { 0 } else { 1 };
    }
    let child = spawn("/bin/vmcheck", &["/bin/vmcheck", "--child", "22222"]);
    if child <= 0 {
        println!("vmcheck: spawn failed");
        return 1;
    }
    let good = hold("parent", 11111, 400);
    let status = wait(child as usize);
    if good && status == 0 {
        println!("vmcheck: separate");
        0
    } else {
        println!("vmcheck: NOT SEPARATE (child status {status})");
        1
    }
}

entry!(main);
