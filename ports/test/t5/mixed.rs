//! The Rust half of t5-mixed, a static library a C++ main links with clang++.
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};

static DROPS: AtomicUsize = AtomicUsize::new(0);

struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

unsafe extern "C-unwind" {
    fn cxx_throw(value: i32);
    fn cxx_call(callback: extern "C-unwind" fn(i32) -> i32, value: i32) -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_sum(values: *const i64, n: usize) -> i64 {
    let v = unsafe { std::slice::from_raw_parts(values, n) };
    v.iter().copied().collect::<Vec<_>>().iter().sum()
}

/// A C++ exception passes through this Rust frame, dropping its guard.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn rust_through(value: i32) {
    let _g = Guard;
    unsafe { cxx_throw(value) };
}

extern "C-unwind" fn rust_panics(value: i32) -> i32 {
    let _g = Guard;
    panic!("rust panic {value} through C++");
}

/// A Rust panic passes through a C++ frame (its destructor runs) and is caught here.
#[unsafe(no_mangle)]
pub extern "C" fn rust_catch() -> i32 {
    panic::set_hook(Box::new(|_| {}));
    let r = panic::catch_unwind(AssertUnwindSafe(|| unsafe { cxx_call(rust_panics, 5) }));
    let _ = panic::take_hook();
    match r {
        Err(e) => {
            let msg = e.downcast_ref::<String>().cloned().unwrap_or_default();
            println!("rust caught: {msg}");
            1
        }
        Ok(_) => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_drops() -> usize {
    DROPS.load(Ordering::SeqCst)
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_sum() -> u64 {
    let handles: Vec<_> = (1..=4u64).map(|i| std::thread::spawn(move || i * 10)).collect();
    handles.into_iter().map(|h| h.join().unwrap()).sum()
}
