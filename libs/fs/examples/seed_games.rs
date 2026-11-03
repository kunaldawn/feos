//! Seeds an exFAT game image from the per-game ZIPs in `data/games/dos`.
//!
//! ```text
//! cargo run -p feos-fs --example seed_games --target x86_64-unknown-linux-gnu -- \
//!     data/games/dos.img data/games/dos
//! ```

use std::env;
use std::fs;

use feos_fs::{mount_image_mut, NodeId};

fn main() {
    let args: Vec<String> = env::args().collect();
    let image_path = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("data/games/dos.img");
    let source = args.get(2).map(String::as_str).unwrap_or("data/games/dos");

    let mut image = fs::read(image_path).expect("read image");
    {
        let mut volume = mount_image_mut(&mut image).expect("mount exFAT image");
        let mut entries: Vec<_> = fs::read_dir(source)
            .expect("read game directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
            })
            .collect();
        entries.sort();
        for path in entries {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let data = fs::read(&path).expect("read zip");
            println!("  {name} ({} bytes)", data.len());
            let node = volume.create_file(NodeId::ROOT, &name).expect("create zip");
            volume.write(node, 0, &data).expect("write zip");
        }
        let games_txt = fs::read("data/games/GAMES.TXT").unwrap_or_default();
        if !games_txt.is_empty() {
            let node = volume
                .create_file(NodeId::ROOT, "GAMES.TXT")
                .expect("create GAMES.TXT");
            volume.write(node, 0, &games_txt).expect("write GAMES.TXT");
        }
        volume.flush().expect("flush image");
    }
    fs::write(image_path, &image).expect("write image");
    println!("seeded {image_path} from {source}");
}
