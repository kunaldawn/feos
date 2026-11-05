//! The smallest program: says who it is, prints its arguments, and can exit with a status.
//!     hello [--exit N] [--quiet] [words...]

#![no_std]
#![no_main]

use feos_user::{entry, getpid, parse_number, println, Args};

fn main(args: Args) -> i32 {
    let status = args.value_of("--exit").and_then(parse_number).unwrap_or(0) as i32;
    if args.has("--quiet") {
        return status;
    }
    println!("hello from user mode: pid {} argc {}", getpid(), args.len());
    for index in 1..args.len() {
        println!("argv[{index}]={}", args.get(index).unwrap_or("?"));
    }
    status
}

entry!(main);
