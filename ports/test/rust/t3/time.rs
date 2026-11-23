//! t3-time: Instant (CLOCK_MONOTONIC), SystemTime (CLOCK_REALTIME) and sleep.
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn main() {
    let start = Instant::now();
    let mut last = start;
    for _ in 0..10_000 {
        let now = Instant::now();
        assert!(now >= last, "Instant went backwards");
        last = now;
    }

    let before = Instant::now();
    thread::sleep(Duration::from_millis(200));
    let slept = before.elapsed();
    assert!(slept >= Duration::from_millis(200), "slept only {slept:?}");
    assert!(slept < Duration::from_secs(10), "slept {slept:?}");
    println!("sleep(200 ms): {} ms", slept.as_millis());

    let before = Instant::now();
    thread::sleep(Duration::from_micros(1500));
    assert!(before.elapsed() >= Duration::from_micros(1500));
    thread::sleep(Duration::ZERO);

    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    // QEMU's clock is the host's: well after 2023.
    assert!(since.as_secs() > 1_700_000_000, "wall clock {since:?}");
    let a = SystemTime::now();
    thread::sleep(Duration::from_millis(50));
    let b = SystemTime::now();
    let d = b.duration_since(a).unwrap();
    assert!(d >= Duration::from_millis(40) && d < Duration::from_secs(10), "wall step {d:?}");
    println!("SystemTime: {} s since the epoch", since.as_secs());

    let later = start.checked_add(Duration::from_secs(1)).unwrap();
    assert_eq!(later.duration_since(start), Duration::from_secs(1));
    assert_eq!(start.checked_duration_since(later), None);
    assert!(start.elapsed() >= Duration::from_millis(250));
    println!("t3-time: ok");
}
