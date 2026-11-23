//! t3-hello: the smallest std program, and the child the process tests spawn.
//!
//!     t3-hello [ARG...]          prints a greeting and its arguments
//!     t3-hello --exit N          exits with status N
//!     t3-hello --sleep SECONDS   sleeps, then exits 0
//!     t3-hello --cwd             prints its working directory
//!     t3-hello --env NAME        prints NAME's value, or "unset"
//!     t3-hello --stderr TEXT     writes TEXT to stderr
//!     t3-hello --panic           panics (exit status 101)
use std::io::Write;
use std::{env, process, thread, time::Duration};

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--exit") => process::exit(args[2].parse().unwrap()),
        Some("--sleep") => {
            thread::sleep(Duration::from_secs(args[2].parse().unwrap()));
            return;
        }
        Some("--cwd") => {
            println!("cwd={}", env::current_dir().unwrap().display());
            return;
        }
        Some("--env") => {
            println!("{}={}", args[2], env::var(&args[2]).unwrap_or_else(|_| "unset".into()));
            return;
        }
        Some("--stderr") => {
            eprintln!("{}", args[2]);
            return;
        }
        Some("--panic") => panic!("t3-hello was asked to panic"),
        _ => {}
    }
    println!("hello from Rust std on feos");
    for (i, arg) in args.iter().enumerate().skip(1) {
        println!("arg[{i}]={arg}");
    }
    std::io::stdout().flush().unwrap();
    println!("t3-hello: ok");
}
