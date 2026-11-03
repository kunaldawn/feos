//! Checks the shipped exFAT game image.
//!
//! `cargo run -p feos-fs --example check_games --target x86_64-unknown-linux-gnu -- data/games/dos.img`

use std::env;
use std::fs;

use feos_fs::mount_image;

const EXPECTED: &[&str] = &[
    "champ-kong.zip",
    "digger-remastered.zip",
    "doom.zip",
    "duke-nukem-3d.zip",
    "galacta-the-battle-for-saturn.zip",
    "mario-bros-vga.zip",
    "quake.zip",
    "simcity-2000.zip",
    "snood.zip",
    "super-street-fighter-ii-turbo.zip",
    "tank-wars.zip",
    "warcraft-2-tides-of-darkness.zip",
    "wolfenstein-3d.zip",
];

fn main() {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "data/games/dos.img".into());
    let image = fs::read(&path).expect("read image");
    let fs = mount_image(&image).expect("mount exFAT image");
    let root = fs.root();
    let mut found = Vec::new();
    for index in 0..fs.child_count(root) {
        if let Some(node) = fs.child_at(root, index) {
            if let Some(name) = fs.name(node) {
                if fs.is_file(node) {
                    found.push(name.to_string());
                }
            }
        }
    }
    found.sort();
    println!("{}: {} files", path, found.len());
    for name in &found {
        println!("  {name}");
    }
    for expected in EXPECTED {
        assert!(
            found.iter().any(|name| name == expected),
            "missing {expected}"
        );
    }
    let games_txt = fs.resolve("GAMES.TXT").expect("GAMES.TXT present");
    assert!(fs.is_file(games_txt));
    println!("GAMES.TXT: {} bytes", fs.size(games_txt));
    println!("OK: all {} game ZIPs are present", EXPECTED.len());
}
