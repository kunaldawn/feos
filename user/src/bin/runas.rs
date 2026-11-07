//! Runs a program as another user: `runas <uid> <gid> <path> [args...]`. Only root can.

#![no_std]
#![no_main]

use feos_user::{entry, eprintln, error_name, parse_number, setuid, spawn, wait, Args};

fn main(args: Args) -> i32 {
    let (Some(uid), Some(gid), Some(path)) = (
        args.get(1).and_then(parse_number),
        args.get(2).and_then(parse_number),
        args.get(3),
    ) else {
        eprintln!("usage: runas UID GID PROGRAM [ARGS...]");
        return 2;
    };
    let result = setuid(uid as usize, gid as usize);
    if result < 0 {
        eprintln!("runas: {}", error_name(result));
        return 1;
    }
    // A program's own name is its first argument, as ever.
    let mut rest = [""; 16];
    rest[0] = path;
    let mut count = 1;
    for index in 4..args.len() {
        if count == rest.len() {
            break;
        }
        rest[count] = args.get(index).unwrap_or("");
        count += 1;
    }
    let pid = spawn(path, &rest[..count]);
    if pid < 0 {
        eprintln!("runas: {path}: {}", error_name(pid));
        return 127;
    }
    wait(pid as usize) as i32
}

entry!(main);
