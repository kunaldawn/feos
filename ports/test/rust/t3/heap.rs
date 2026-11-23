//! t3-heap: the allocator under load (std's System allocator is relibc's malloc,
//! dlmalloc over MMAP2), and HashMap's random keys.
use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::BuildHasher;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--seed") {
        // For the parent's check: this program's AT_RANDOM and its first map's hash.
        println!("{} {:x}", hex(&at_random()), RandomState::new().hash_one("feos"));
        return;
    }
    let mut rng = Rng(0x9e3779b97f4a7c15);
    // 200k blocks of up to 4 KiB, freed in random order, each checked by pattern.
    let mut blocks: Vec<Option<Vec<u8>>> = Vec::new();
    for round in 0..200_000u64 {
        let size = (rng.next() % 4096) as usize + 1;
        let tag = (round % 251) as u8;
        blocks.push(Some(vec![tag; size]));
        if blocks.len() > 2000 {
            let i = (rng.next() as usize) % blocks.len();
            if let Some(b) = blocks[i].take() {
                assert!(b.iter().all(|&x| x == b[0]), "a block changed under us");
            }
        }
        if blocks.len() > 4000 {
            blocks.retain(Option::is_some);
        }
    }
    for b in blocks.into_iter().flatten() {
        assert!(b.iter().all(|&x| x == b[0]));
    }
    println!("200000 small blocks: ok");

    // Growth by realloc to 64 MiB and back down; one 128 MiB block touched per page.
    let mut v: Vec<u64> = Vec::new();
    for i in 0..(8 << 20) {
        v.push(i);
    }
    assert_eq!(v.len() * 8, 64 << 20);
    assert!(v.iter().enumerate().all(|(i, &x)| x == i as u64));
    v.truncate(1000);
    v.shrink_to_fit();
    assert_eq!(v.iter().sum::<u64>(), 999 * 1000 / 2);
    let mut big = vec![0u8; 128 << 20];
    for i in (0..big.len()).step_by(4096) {
        big[i] = (i >> 12) as u8;
    }
    assert!((0..big.len()).step_by(4096).all(|i| big[i] == (i >> 12) as u8));
    drop(big);
    println!("64 MiB vector, 128 MiB block: ok");

    // Maps and strings.
    let mut map = HashMap::new();
    let mut tree = BTreeMap::new();
    for i in 0..100_000u64 {
        map.insert(i, i * 2);
        tree.insert(i.wrapping_mul(0x9e3779b97f4a7c15), i);
    }
    assert_eq!(map.len(), 100_000);
    assert!((0..100_000u64).all(|i| map[&i] == i * 2));
    assert_eq!(tree.len(), 100_000);
    let mut s = String::new();
    for i in 0..50_000 {
        s.push_str(&i.to_string());
    }
    assert_eq!(s.len(), (0..50_000).map(|i: i32| i.to_string().len()).sum::<usize>());
    println!("HashMap and BTreeMap of 100000, a {} byte string: ok", s.len());

    // The seed under RandomState. One thread's maps always differ (std adds 1 to a
    // cached key per map), so this checks what that cache is filled from: the
    // kernel's AT_RANDOM bytes (present, not all zero, different in every program),
    // and hashmap_random_keys (GRND_INSECURE, or before 1e.1 AT_RANDOM mixed with the
    // clocks) called afresh by each thread and by each process.
    let seed = at_random();
    assert!(seed.iter().any(|&b| b != 0), "AT_RANDOM is all zero");
    let threads: HashSet<u64> = (0..8)
        .map(|_| std::thread::spawn(|| RandomState::new().hash_one("feos")).join().unwrap())
        .collect();
    assert_eq!(threads.len(), 8, "threads' RandomState keys repeat");
    let exe = std::env::current_exe().unwrap();
    let children: Vec<String> = (0..2)
        .map(|_| {
            let out = std::process::Command::new(&exe).arg("--seed").output().unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap()
        })
        .collect();
    let (a, b) = (&children[0], &children[1]);
    let field = |s: &str, n: usize| s.split_whitespace().nth(n).unwrap().to_owned();
    assert_ne!(field(a, 0), field(b, 0), "two programs got the same AT_RANDOM");
    assert_ne!(field(a, 0), hex(&seed), "a child got its parent's AT_RANDOM");
    assert_ne!(field(a, 1), field(b, 1), "two programs' RandomState keys are the same");
    println!("RandomState: AT_RANDOM differs per program, keys differ per thread (8) and per program (2)");

    // Threads allocating at once.
    let hs: Vec<_> = (0..4)
        .map(|t| {
            std::thread::spawn(move || {
                let mut keep = Vec::new();
                for i in 0..20_000usize {
                    keep.push(vec![t as u8; (i % 513) + 1]);
                    if keep.len() > 100 {
                        keep.swap_remove(i % 100);
                    }
                }
                keep.iter().all(|b| b.iter().all(|&x| x == t as u8))
            })
        })
        .collect();
    assert!(hs.into_iter().all(|h| h.join().unwrap()));
    println!("t3-heap: ok");
}

const AT_RANDOM: u64 = 25;
extern "C" {
    // relibc's, linked statically (no libc crate outside std).
    fn getauxval(kind: u64) -> u64;
}

/// The 16 bytes the kernel put at AT_RANDOM in this program's aux vector.
fn at_random() -> [u8; 16] {
    let at = unsafe { getauxval(AT_RANDOM) } as *const [u8; 16];
    assert!(!at.is_null(), "no AT_RANDOM in the aux vector");
    unsafe { *at }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
