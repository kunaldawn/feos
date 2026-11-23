//! Shows two processes holding different data at the same virtual address.
//!
//! A static and an `mmap`ed page have the same address in every process running this.
//! The parent starts a child, which writes its own values at the same addresses and
//! waits to be overlapped; each then checks what it reads is still its own.
//!
//! With `--fork` the child is a `FORK` of the parent (stage 1d.1): the page is mapped
//! once, before it, and both write it, the same frame copied on the first write.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU64, Ordering};

use feos_user::{abi, entry, getpid, mmap, parse_number, println, sleep_ms, spawn, wait, Args};

static VALUE: AtomicU64 = AtomicU64::new(0);

/// A page for [`hold`].
fn page(role: &str) -> Option<*mut u64> {
    let page = mmap(abi::PAGE_SIZE, abi::prot::READ | abi::prot::WRITE);
    if page <= 0 {
        println!("vmcheck {role}: mmap failed");
        return None;
    }
    Some(page as usize as *mut u64)
}

/// Writes `value` to both places, holds it a while, and says whether it's still there.
fn hold(role: &str, cell: *mut u64, value: u64, ms: usize) -> bool {
    VALUE.store(value, Ordering::SeqCst);
    // SAFETY: a page mapped for this.
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

/// The parent's end: its own values held, and the child's status.
fn finish(good: bool, status: isize) -> i32 {
    if good && status == 0 {
        println!("vmcheck: separate");
        0
    } else {
        println!("vmcheck: NOT SEPARATE (child status {status})");
        1
    }
}

fn main(args: Args) -> i32 {
    if let Some(value) = args.value_of("--child").and_then(parse_number) {
        return match page("child") {
            Some(cell) if hold("child", cell, value as u64, 400) => 0,
            _ => 1,
        };
    }
    let Some(cell) = page("parent") else {
        return 1;
    };
    if args.has("--fork") {
        // Written before the fork, so both start from the same frame.
        VALUE.store(1, Ordering::SeqCst);
        // SAFETY: just mapped, for this.
        unsafe { cell.write_volatile(1) };
        let child = feos_user::fork();
        if child == 0 {
            feos_user::exit(if hold("child", cell, 22222, 400) { 0 } else { 1 });
        }
        if child < 0 {
            println!("vmcheck: fork failed: {child}");
            return 1;
        }
        let good = hold("parent", cell, 11111, 400);
        return finish(good, feos_user::wait_status(child));
    }
    let child = spawn("/bin/vmcheck", &["/bin/vmcheck", "--child", "22222"]);
    if child <= 0 {
        println!("vmcheck: spawn failed");
        return 1;
    }
    let good = hold("parent", cell, 11111, 400);
    finish(good, wait(child as usize))
}

entry!(main);
