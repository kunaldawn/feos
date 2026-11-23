//! t3-panic: unwinding (panic_unwind over LLVM's libunwind, frames found through
//! relibc's dl_iterate_phdr and the program's PT_GNU_EH_FRAME).
use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

static DROPS: AtomicUsize = AtomicUsize::new(0);
static HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);

struct Guard(usize);
impl Drop for Guard {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
        let _ = self.0;
    }
}

#[inline(never)]
fn deep(depth: usize) -> usize {
    let _guard = Guard(depth);
    if depth == 0 {
        panic!("bottom of the stack at depth {depth}");
    }
    // Not a tail call: the guard lives across it.
    deep(std::hint::black_box(depth - 1)) + 1
}

fn message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else {
        "?".into()
    }
}

fn main() {
    // A panic unwinds through 21 frames, dropping each one's guard, to catch_unwind.
    let r = panic::catch_unwind(|| deep(20));
    let payload = r.unwrap_err();
    assert_eq!(message(&*payload), "bottom of the stack at depth 0");
    assert_eq!(DROPS.load(Ordering::SeqCst), 21, "drops while unwinding");
    println!("catch_unwind: 21 frames unwound, 21 destructors ran");

    // A hook sees every panic; resume_unwind carries any payload and skips the hook.
    let previous = panic::take_hook();
    panic::set_hook(Box::new(|info| {
        HOOK_CALLS.fetch_add(1, Ordering::SeqCst);
        assert!(info.location().is_some());
    }));
    let r = panic::catch_unwind(|| panic!("hooked"));
    assert!(r.is_err());
    let r = panic::catch_unwind(|| panic::resume_unwind(Box::new(1234u32)));
    assert_eq!(*r.unwrap_err().downcast::<u32>().unwrap(), 1234);
    assert_eq!(HOOK_CALLS.load(Ordering::SeqCst), 1);
    println!("hook: 1 call; resume_unwind carried 1234");

    // A panicking thread's join is its payload; the others go on.
    let h = thread::spawn(|| {
        let _g = Guard(99);
        panic!("in a thread");
    });
    let e = h.join().unwrap_err();
    assert_eq!(message(&*e), "in a thread");
    let ok = thread::spawn(|| 5).join().unwrap();
    assert_eq!(ok, 5);
    println!("thread panic: joined with its payload");

    // Nested: a panic caught inside a frame that is itself unwinding a caught panic.
    let r = panic::catch_unwind(AssertUnwindSafe(|| {
        let inner = panic::catch_unwind(|| panic!("inner"));
        assert!(inner.is_err());
        panic!("outer");
    }));
    assert_eq!(message(&*r.unwrap_err()), "outer");
    // Unwinding out of an arithmetic overflow check and an index check.
    let v: Vec<u8> = vec![1, 2, 3];
    let i = std::hint::black_box(7);
    assert!(panic::catch_unwind(|| v[i]).is_err());
    let x = std::hint::black_box(u8::MAX);
    assert!(panic::catch_unwind(|| x.checked_add(1).expect("overflow")).is_err());
    panic::set_hook(previous);
    println!("nested, index, expect: ok");

    // The default hook's report, on stderr, once more for the record.
    let r = panic::catch_unwind(|| panic!("reported by the default hook"));
    assert!(r.is_err());
    println!("t3-panic: ok");
}
