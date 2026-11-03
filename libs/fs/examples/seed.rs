//! Seeds a newly formatted FAT32 or exFAT image with sample files.
//!
//! Used by the Docker test images: `seed <fat32|exfat> <image>`.

use std::env;
use std::fs;

use feos_fs::{mount_image_mut, NodeId};

fn main() {
    let kind = env::args().nth(1).expect("filesystem kind");
    let path = env::args().nth(2).expect("image path");
    let label = match kind.as_str() {
        "fat32" => "FAT32",
        "exfat" => "exFAT",
        other => panic!("unknown filesystem kind: {other}"),
    };

    let mut image = fs::read(&path).unwrap();
    {
        let mut fs = mount_image_mut(&mut image).unwrap();

        let hello = fs.create_file(NodeId::ROOT, "hello.txt").unwrap();
        fs.write(hello, 0, format!("Hello from {label}!\n").as_bytes())
            .unwrap();

        let docs = fs.create_dir(NodeId::ROOT, "docs").unwrap();
        let notes = fs.create_file(docs, "notes.md").unwrap();
        fs.write(
            notes,
            0,
            b"# notes\n\nThis file lives on a real disk image.\n",
        )
        .unwrap();

        let big_data: Vec<u8> = (0..1500).map(|index| (index % 251) as u8).collect();
        let big = fs.create_file(NodeId::ROOT, "big.bin").unwrap();
        fs.write(big, 0, &big_data).unwrap();

        if kind == "fat32" {
            let long = fs.create_file(NodeId::ROOT, "Long File Name.txt").unwrap();
            fs.write(long, 0, b"A long FAT32 file name.\n").unwrap();
        } else {
            let unicode = fs.create_file(NodeId::ROOT, "café.txt").unwrap();
            fs.write(unicode, 0, "A Unicode exFAT file name.\n".as_bytes())
                .unwrap();
        }

        fs.flush().unwrap();
    }
    fs::write(&path, &image).unwrap();
    println!("seeded {path} ({kind})");
}
