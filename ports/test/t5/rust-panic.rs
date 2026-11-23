//! t5-rust-panic: a panic unwound by the LLVM libunwind ports/build-runtimes.sh built
//! (the sysroot's libunwind.a, linked in place of rust's own copy), caught by
//! catch_unwind, on the main thread and on another.
use std::panic;
use std::sync::atomic::{AtomicUsize, Ordering};

static DROPS: AtomicUsize = AtomicUsize::new(0);

struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

#[inline(never)]
fn deep(n: usize) -> usize {
    let _g = Guard;
    if n == 0 {
        panic!("deep panic");
    }
    deep(std::hint::black_box(n - 1)) + 1
}

fn main() {
    panic::set_hook(Box::new(|info| eprintln!("hook: {info}")));
    let r = panic::catch_unwind(|| deep(15));
    let msg = r.unwrap_err().downcast::<&str>().map(|s| *s).unwrap_or("?");
    println!("caught: {msg}, drops {}", DROPS.load(Ordering::SeqCst));
    let t = std::thread::spawn(|| panic::catch_unwind(|| deep(3)).is_err());
    println!("thread caught: {}", t.join().unwrap());
    let joined = std::thread::spawn(|| deep(2)).join();
    println!("join error: {}", joined.is_err());
    assert_eq!(DROPS.load(Ordering::SeqCst), 16 + 4 + 3);
    println!("t5-rust-panic: ok");
}
