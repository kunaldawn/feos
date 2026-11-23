//! t3-overflow: a stack overflow is reported and aborts (std's SIGSEGV handler on an
//! alternate stack, guard ranges from pthread_getattr_np), on the main thread or another.
//!
//!     t3-overflow main | thread
use std::hint::black_box;

#[inline(never)]
fn recurse(n: u64) -> u64 {
    let buf = black_box([n; 64]);
    if n == u64::MAX {
        return 0;
    }
    recurse(black_box(n + 1)) + buf[3]
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("main") => {
            println!("overflowing the main thread's stack");
            black_box(recurse(0));
        }
        Some("thread") => {
            println!("overflowing a thread's stack");
            std::thread::Builder::new()
                .name("t3-overflow-thread".into())
                .stack_size(128 * 1024)
                .spawn(|| black_box(recurse(0)))
                .unwrap()
                .join()
                .unwrap();
        }
        _ => panic!("usage: t3-overflow main|thread"),
    }
    unreachable!("the stack never overflowed");
}
