//! Copies files, or standard input, to standard output.

#![no_std]
#![no_main]

use feos_user::{abi, close, entry, eprintln, error_name, open, read, write, Args};

fn copy(fd: usize, name: &str) -> i32 {
    let mut buffer = [0u8; 4096];
    loop {
        let got = read(fd, &mut buffer);
        if got == 0 {
            return 0;
        }
        if got < 0 {
            eprintln!("cat: {name}: {}", error_name(got));
            return 1;
        }
        let mut sent = 0;
        while sent < got as usize {
            let wrote = write(1, &buffer[sent..got as usize]);
            if wrote <= 0 {
                return 1;
            }
            sent += wrote as usize;
        }
    }
}

fn main(args: Args) -> i32 {
    if args.len() < 2 {
        return copy(0, "stdin");
    }
    let mut status = 0;
    for index in 1..args.len() {
        let name = args.get(index).unwrap_or("");
        if name == "-" {
            status |= copy(0, "stdin");
            continue;
        }
        let fd = open(name, abi::open::READ);
        if fd < 0 {
            eprintln!("cat: {name}: {}", error_name(fd));
            status = 1;
            continue;
        }
        status |= copy(fd as usize, name);
        close(fd as usize);
    }
    status
}

entry!(main);
