//! POSIX-style core utilities for FEOS.
//!
//! This crate is the small-system equivalent of GNU coreutils: it contains the
//! file, text and system commands the shell dispatches, but it does not own a
//! console, a board or a process table. Commands receive a [`Vfs`] view, an
//! [`Env`], byte streams and a [`System`] implementation through [`Ctx`].
//!
//! The utilities are inspired by [uutils/coreutils](https://github.com/uutils/coreutils)
//! and are deliberately portable: the same code runs in the kernel and in
//! host-side tests.
//!
//! # Example
//!
//! ```
//! use feos_coreutils::{
//!     BytesOutput, Ctx, Env, NoSystem, NullInput, OverlayFs, Vfs, run,
//! };
//!
//! let mut fs = OverlayFs::memory();
//! let mut env = Env::feos();
//! let mut stdin = NullInput;
//! let mut stdout = BytesOutput::new();
//! let mut stderr = BytesOutput::new();
//! let mut ctx = Ctx {
//!     fs: &mut fs,
//!     env: &mut env,
//!     stdin: &mut stdin,
//!     stdout: &mut stdout,
//!     stderr: &mut stderr,
//!     system: &NoSystem,
//!     depth: 0,
//! };
//! assert_eq!(run("mkdir", &["docs".into()], &mut ctx), 0);
//! assert!(fs.is_dir("/docs"));
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod commands;
pub mod env;
pub mod fs;
pub mod glob;
pub mod io;
pub mod regex;
pub mod sha256;
pub mod system;

pub use commands::{command, is_command, run, CommandFn, CommandSpec, Ctx, COMMANDS};
pub use env::{Env, Var, VarError};
pub use fs::{base_name, join, normalize, parent, DirEntry, FsError, Kind, Meta, OverlayFs, Vfs};
pub use io::{Available, BytesInput, BytesOutput, Input, NullInput, NullOutput, Output, MAX_STREAM_BYTES};
pub use system::{MemoryInfo, NoSystem, System};
