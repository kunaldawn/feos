//! t3-threads: std::thread and std::sync on feos (relibc's pthreads, std's futex
//! locks over relibc's __feos_futex_wait/wake, TLS through SET_TLS).
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier, Condvar, LazyLock, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::Duration;

static DROPPED: AtomicUsize = AtomicUsize::new(0);

struct Tracked(usize);
impl Drop for Tracked {
    fn drop(&mut self) {
        DROPPED.fetch_add(self.0, Ordering::SeqCst);
    }
}

thread_local! {
    static COUNT: RefCell<u32> = const { RefCell::new(0) };
    static TRACKED: Tracked = Tracked(1);
}

static LAZY: LazyLock<u64> = LazyLock::new(|| 6 * 7);

fn main() {
    let n = thread::available_parallelism().unwrap().get();
    println!("available_parallelism: {n}");

    // Spawn, join, Mutex.
    let counter = Arc::new(Mutex::new(0u64));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let counter = Arc::clone(&counter);
            thread::spawn(move || {
                for _ in 0..10_000 {
                    *counter.lock().unwrap() += 1;
                }
                i * 10
            })
        })
        .collect();
    let returned: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(*counter.lock().unwrap(), 80_000);
    assert_eq!(returned, (0..8).map(|i| i * 10).sum());
    println!("mutex: 8 threads x 10000 = {}", counter.lock().unwrap());

    // Condvar: a producer and a consumer.
    let pair = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
    let consumer = {
        let pair = Arc::clone(&pair);
        thread::spawn(move || {
            let (queue, ready) = &*pair;
            let mut sum = 0u64;
            loop {
                let mut q = queue.lock().unwrap();
                while q.is_empty() {
                    q = ready.wait(q).unwrap();
                }
                let v = q.pop_front().unwrap();
                if v == 0 {
                    return sum;
                }
                sum += v;
            }
        })
    };
    for v in (1..=1000).chain([0]) {
        let (queue, ready) = &*pair;
        queue.lock().unwrap().push_back(v);
        ready.notify_one();
    }
    assert_eq!(consumer.join().unwrap(), 500_500);
    // wait_timeout times out.
    let (m, c) = (Mutex::new(()), Condvar::new());
    let (_g, timeout) = c.wait_timeout(m.lock().unwrap(), Duration::from_millis(50)).unwrap();
    assert!(timeout.timed_out());
    println!("condvar: sum 500500, wait_timeout timed out");

    // mpsc.
    let (tx, rx) = mpsc::channel();
    for p in 0..4u64 {
        let tx = tx.clone();
        thread::spawn(move || {
            for i in 0..1000 {
                tx.send(p * 1000 + i).unwrap();
            }
        });
    }
    drop(tx);
    let total: u64 = rx.iter().sum();
    assert_eq!(total, (0..4000).sum());
    let (stx, srx) = mpsc::sync_channel(0);
    let t = thread::spawn(move || stx.send("rendezvous").unwrap());
    assert_eq!(srx.recv().unwrap(), "rendezvous");
    t.join().unwrap();
    assert!(srx.recv_timeout(Duration::from_millis(20)).is_err());
    println!("mpsc: 4 producers, sum {total}; sync_channel(0)");

    // thread_local, with destructors at thread exit.
    COUNT.with(|c| *c.borrow_mut() = 100);
    let handles: Vec<_> = (0..4)
        .map(|i| {
            thread::spawn(move || {
                TRACKED.with(|t| assert_eq!(t.0, 1));
                for _ in 0..=i {
                    COUNT.with(|c| *c.borrow_mut() += 1);
                }
                COUNT.with(|c| *c.borrow())
            })
        })
        .collect();
    let counts: Vec<u32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(counts, [1, 2, 3, 4]);
    assert_eq!(COUNT.with(|c| *c.borrow()), 100);
    assert_eq!(DROPPED.load(Ordering::SeqCst), 4, "thread-local destructors");
    println!("thread_local: {counts:?}, 4 destructors ran");

    // Barrier.
    let barrier = Arc::new(Barrier::new(6));
    let leaders: usize = (0..6)
        .map(|_| {
            let b = Arc::clone(&barrier);
            thread::spawn(move || b.wait().is_leader() as usize)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| h.join().unwrap())
        .sum();
    assert_eq!(leaders, 1);
    println!("barrier: 6 threads, 1 leader");

    // RwLock, OnceLock, LazyLock.
    let lock = Arc::new(RwLock::new(vec![1, 2, 3]));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let l = Arc::clone(&lock);
            thread::spawn(move || l.read().unwrap().iter().sum::<i32>())
        })
        .collect();
    lock.write().unwrap().push(4);
    for r in readers {
        let s = r.join().unwrap();
        assert!(s == 6 || s == 10);
    }
    static ONCE: OnceLock<String> = OnceLock::new();
    let a = thread::spawn(|| ONCE.get_or_init(|| "first".into()).clone()).join().unwrap();
    assert_eq!(ONCE.get_or_init(|| "second".into()), &a);
    assert_eq!(*LAZY, 42);
    println!("rwlock, once, lazy: ok");

    // Builder: name and stack size; scoped threads; park/unpark.
    let named = thread::Builder::new()
        .name("t3-worker".into())
        .stack_size(256 * 1024)
        .spawn(|| thread::current().name().map(str::to_owned))
        .unwrap();
    assert_eq!(named.join().unwrap().as_deref(), Some("t3-worker"));
    // The smallest stack std asks for is PTHREAD_STACK_MIN (16 KiB, section 10.2),
    // and a thread runs (and formats) on it.
    let small = thread::Builder::new()
        .stack_size(1)
        .spawn(|| (own_stack_size(), format!("{:?}", (1..=10).collect::<Vec<u32>>())))
        .unwrap();
    let (size, text) = small.join().unwrap();
    assert_eq!(text, "[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]");
    assert_eq!(size, 16384, "stack_size(1) gave a {size} byte stack");
    assert_eq!(unsafe { sysconf(_SC_THREAD_STACK_MIN) }, 16384);
    println!("stack_size(1): a {size} byte stack");
    let mut data = [1, 2, 3, 4];
    thread::scope(|s| {
        for x in &mut data {
            s.spawn(move || *x *= 10);
        }
    });
    assert_eq!(data, [10, 20, 30, 40]);
    let parked = thread::spawn(|| {
        thread::park();
        "unparked"
    });
    thread::sleep(Duration::from_millis(20));
    parked.thread().unpark();
    assert_eq!(parked.join().unwrap(), "unparked");
    println!("builder, scope, park: ok");

    // Many short threads (each stack mapped and freed by THREAD_EXIT).
    for round in 0..20 {
        let hs: Vec<_> = (0..10).map(|i| thread::spawn(move || round * 10 + i)).collect();
        for h in hs {
            h.join().unwrap();
        }
    }
    println!("200 short threads: ok");
    println!("t3-threads: ok");
}

// relibc's, linked statically (no libc crate outside std).
#[repr(C, align(8))]
struct PthreadAttr([u8; 32]);
const _SC_THREAD_STACK_MIN: i32 = 75;
extern "C" {
    fn pthread_self() -> *mut u8;
    fn pthread_getattr_np(thread: *mut u8, attr: *mut PthreadAttr) -> i32;
    fn pthread_attr_getstacksize(attr: *const PthreadAttr, size: *mut usize) -> i32;
    fn pthread_attr_destroy(attr: *mut PthreadAttr) -> i32;
    fn sysconf(name: i32) -> i64;
}

/// The calling thread's usable stack size, as pthread_getattr_np reports it.
fn own_stack_size() -> usize {
    let mut attr = PthreadAttr([0; 32]);
    let mut size = 0;
    unsafe {
        assert_eq!(pthread_getattr_np(pthread_self(), &mut attr), 0);
        assert_eq!(pthread_attr_getstacksize(&attr, &mut size), 0);
        pthread_attr_destroy(&mut attr);
    }
    size
}
