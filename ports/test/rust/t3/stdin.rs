//! t3-stdin: reads all of std::io::stdin and reports it (the process test pipes into it).
use std::io::{self, BufRead, Read};

fn main() {
    let mut lines = 0;
    let mut bytes = 0;
    let stdin = io::stdin();
    let mut first = String::new();
    let n = stdin.lock().read_line(&mut first).unwrap();
    if n > 0 {
        lines += 1;
        bytes += n;
    }
    let mut rest = String::new();
    bytes += stdin.lock().read_to_string(&mut rest).unwrap();
    lines += rest.lines().count();
    println!("t3-stdin: {lines} lines, {bytes} bytes, first {:?}", first.trim_end());
}
