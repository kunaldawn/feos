//! Every cursor in `assets/cursors` decodes and agrees with the index, and
//! damaged copies of them fail without panicking.

use std::path::PathBuf;

use feos_image::ani::Animation;
use feos_image::cur;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/cursors")
}

struct Row {
    path: String,
    size: (u32, u32),
    hotspot: (u32, u32),
    frames: u32,
    steps: u32,
}

fn index() -> Vec<Row> {
    let text = std::fs::read_to_string(root().join("index.tsv")).unwrap();
    text.lines()
        .skip(1)
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let pair = |text: &str, separator: char| {
                let (a, b) = text.split_once(separator).unwrap();
                (a.parse().unwrap(), b.parse().unwrap())
            };
            Row {
                path: fields[0].to_string(),
                size: pair(fields[3], 'x'),
                hotspot: pair(fields[4], ','),
                frames: fields[5].parse().unwrap(),
                steps: fields[6].parse().unwrap(),
            }
        })
        .collect()
}

/// Decodes a whole file, every frame, returning the first frame's info and
/// how many pixels are opaque or inverting in it.
fn decode_all(data: &[u8]) -> Result<(cur::CursorInfo, u32, usize), feos_image::ImageError> {
    let mut rgba = vec![0u8; 256 * 256 * 4];
    let mut invert = vec![0u8; 256 * 256 / 8];
    if cur::is_magic(data) {
        let info = cur::decode(data, &mut rgba, &mut invert, &mut [])?;
        let visible = count_visible(&rgba, &invert, &info);
        return Ok((info, 1, visible));
    }
    let animation = Animation::parse(data)?;
    let mut first = None;
    for frame in 0..animation.frames() {
        let info = animation.decode_frame(frame, &mut rgba, &mut invert, &mut [])?;
        if first.is_none() {
            first = Some((info, count_visible(&rgba, &invert, &info)));
        }
    }
    for step in 0..animation.steps() {
        assert!(animation.frame_at_step(step) < animation.frames());
    }
    let (info, visible) = first.unwrap();
    Ok((info, animation.steps(), visible))
}

fn count_visible(rgba: &[u8], invert: &[u8], info: &cur::CursorInfo) -> usize {
    let pixels = (info.width * info.height) as usize;
    (0..pixels)
        .filter(|&index| rgba[index * 4 + 3] != 0 || cur::inverts(invert, index))
        .count()
}

#[test]
fn every_cursor_matches_the_index() {
    let rows = index();
    assert_eq!(rows.len(), 284);
    for row in &rows {
        let data = std::fs::read(root().join(&row.path)).unwrap();
        let (info, steps, visible) =
            decode_all(&data).unwrap_or_else(|error| panic!("{}: {error:?}", row.path));
        assert_eq!((info.width, info.height), row.size, "{}", row.path);
        assert_eq!((info.hotspot_x, info.hotspot_y), row.hotspot, "{}", row.path);
        assert_eq!(steps, row.steps, "{}", row.path);
        if row.path.ends_with(".ani") {
            assert_eq!(Animation::parse(&data).unwrap().frames(), row.frames, "{}", row.path);
        }
        assert!(visible > 10, "{} shows almost nothing", row.path);
    }
}

#[test]
fn the_arrow_points_from_its_hot_spot_and_the_beam_inverts() {
    let data = std::fs::read(root().join("xp-default/arrow.cur")).unwrap();
    let mut rgba = [0u8; 32 * 32 * 4];
    let info = cur::decode(&data, &mut rgba, &mut [], &mut []).unwrap();
    let at = ((info.hotspot_y * 32 + info.hotspot_x) * 4) as usize;
    assert_eq!(&rgba[at..at + 4], &[0, 0, 0, 255], "the tip is black");
    // Above and left of the tip is clear.
    assert_eq!(rgba[at - 4 + 3], 0);
    assert_eq!(rgba[at - 32 * 4 + 3], 0);

    let data = std::fs::read(root().join("xp-default/text.cur")).unwrap();
    let mut invert = [0u8; 128];
    let info = cur::decode(&data, &mut rgba, &mut invert, &mut []).unwrap();
    let inverting = (0..1024).filter(|&index| cur::inverts(&invert, index)).count();
    assert!(inverting > 10);
    assert!(cur::inverts(&invert, (info.hotspot_y * 32 + info.hotspot_x) as usize));
}

#[test]
fn the_busy_cursor_animates() {
    let data = std::fs::read(root().join("animated/busy.ani")).unwrap();
    let animation = Animation::parse(&data).unwrap();
    assert_eq!(animation.frames(), 15);
    let (first, _) = animation.step_at(0);
    let (later, _) = animation.step_at(animation.cycle_ms() / 2);
    assert_ne!(first, later);
    assert!(animation.cycle_ms() > 1000 && animation.cycle_ms() < 10_000);
}

/// A small deterministic generator, so a failure can be replayed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit.max(1) as u64) as usize
    }
}

#[test]
fn damaged_cursors_fail_without_panicking() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for row in index() {
        let data = std::fs::read(root().join(&row.path)).unwrap();
        // Truncations at a spread of points, including inside every header.
        for _ in 0..24 {
            let end = rng.below(data.len());
            let _ = decode_all(&data[..end]);
        }
        for end in 0..data.len().min(160) {
            let _ = decode_all(&data[..end]);
        }
        // Byte and word mutations, weighted towards the headers and chunk sizes.
        for _ in 0..48 {
            let mut bent = data.clone();
            for _ in 0..1 + rng.below(4) {
                let at = if rng.below(2) == 0 { rng.below(96.min(bent.len())) } else { rng.below(bent.len()) };
                match rng.below(4) {
                    0 => bent[at] ^= 1 << rng.below(8),
                    1 => bent[at] = 0xff,
                    2 => bent[at] = 0,
                    _ => {
                        let end = (at + 4).min(bent.len());
                        bent[at..end].fill(0xff);
                    }
                }
            }
            let _ = decode_all(&bent);
        }
    }
}
