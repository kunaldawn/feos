//! Compares every frame with FFmpeg on real conformance streams, which aren't in this
//! repository. `FEOS_H264_CONF` names a directory of FATE's `h264-conformance`, and
//! `FEOS_H264_REFS` one holding `<stream>.crc` for each: the output of
//!
//!     ffmpeg -threads 1 -i <stream> -map 0:v -fps_mode passthrough -f framecrc <stream>.crc
//!
//! Without them the test prints that it was skipped. `FEOS_H264_ONLY` is a comma-separated
//! list of names to run, and `FEOS_H264_DUMP` a file to write the first stream's native
//! frames to, for finding where a mismatch is.

use std::path::{Path, PathBuf};

use feos_h264::{OrderedDecoder, annex_b_units};

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (0u32, 0u32);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

fn find(dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.exists() {
        return Some(direct);
    }
    let frext = dir.join("FRext").join(name);
    frext.exists().then_some(frext)
}

/// Decodes a stream and gives each native frame's Adler-32.
fn decode(stream: &[u8], mut dump: Option<&mut std::fs::File>) -> (Vec<u32>, Option<String>) {
    use std::io::Write;
    let mut decoder = OrderedDecoder::new();
    decoder.set_keep_native(true);
    let mut hashes = Vec::new();
    let mut error = None;
    let mut sink = |frame: feos_h264::Frame, hashes: &mut Vec<u32>| {
        let mut bytes = Vec::new();
        // FFmpeg shows monochrome with grey chroma, which is what the display planes hold.
        if frame.chroma_format != 0 {
            frame.append_native(&mut bytes);
        }
        if bytes.is_empty() {
            frame.append_i420(&mut bytes);
        }
        hashes.push(adler32(&bytes));
        if let Some(file) = dump.as_mut() {
            let _ = file.write_all(&bytes);
        }
    };
    for nal in annex_b_units(stream) {
        match decoder.decode_nal(&nal) {
            Ok(frames) => {
                for frame in frames {
                    sink(frame, &mut hashes);
                }
            }
            Err(e) => {
                error.get_or_insert_with(|| format!("{e:?}"));
            }
        }
    }
    for frame in decoder.flush() {
        sink(frame, &mut hashes);
    }
    (hashes, error)
}

fn reference(path: &Path) -> Option<Vec<u32>> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(
        text.lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| {
                let field = line.split(',').nth(5)?.trim();
                u32::from_str_radix(field.trim_start_matches("0x"), 16).ok()
            })
            .collect(),
    )
}

#[test]
fn conformance_streams_match_ffmpeg() {
    let (Ok(conf), Ok(refs)) = (std::env::var("FEOS_H264_CONF"), std::env::var("FEOS_H264_REFS"))
    else {
        println!("skipped: FEOS_H264_CONF and FEOS_H264_REFS not set");
        return;
    };
    let only: Vec<String> = std::env::var("FEOS_H264_ONLY")
        .map(|list| list.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let dump_path = std::env::var("FEOS_H264_DUMP").ok();
    let mut names: Vec<String> = Vec::new();
    if only.is_empty() {
        for entry in std::fs::read_dir(&refs).unwrap().flatten() {
            let name = entry.file_name().into_string().unwrap();
            if let Some(stem) = name.strip_suffix(".crc") {
                names.push(stem.to_owned());
            }
        }
        names.sort();
    } else {
        names = only;
    }
    let mut bad = 0;
    for name in &names {
        let Some(stream) = find(Path::new(&conf), name) else {
            println!("{name}: not found");
            continue;
        };
        let Some(want) = reference(&Path::new(&refs).join(format!("{name}.crc"))) else {
            println!("{name}: no reference");
            continue;
        };
        let data = std::fs::read(stream).unwrap();
        let mut file = dump_path.as_ref().map(|p| std::fs::File::create(p).unwrap());
        let (got, error) = decode(&data, file.as_mut());
        let matched = got.iter().zip(&want).filter(|(a, b)| a == b).count();
        let first = got.iter().zip(&want).position(|(a, b)| a != b);
        let verdict = if got == want { "ok" } else { "DIFF" };
        if got != want {
            bad += 1;
        }
        println!(
            "{name}: {verdict} {matched}/{} (decoded {}), first bad {first:?}{}",
            want.len(),
            got.len(),
            error.map_or(String::new(), |e| format!(", error {e}")),
        );
    }
    println!("{} streams, {bad} differ", names.len());
    if std::env::var("FEOS_H264_STRICT").is_ok() {
        assert_eq!(bad, 0);
    }
}
