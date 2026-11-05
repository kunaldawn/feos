//! Generates this board's embedded filesystem image: see
//! `boards/common/fs_image_build.rs`.

#[path = "../common/fs_image_build.rs"]
mod fs_image_build;

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=../common/fs_image_build.rs");
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    fs_image_build::generate(&manifest_dir);
}
