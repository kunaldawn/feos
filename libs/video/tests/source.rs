//! Decoding a file through a `Source` gives what decoding it from memory gives.

use feos_video::{audio_track, audio_track_from_source, DecoderLimits, Source, VideoDecoder};

const MEDIA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../boards/qemu-virt/fs/media");

/// A file that counts how much is read from it, and refuses to be read all at once.
struct Counting {
    data: Vec<u8>,
    read: std::cell::Cell<u64>,
}

impl Source for Counting {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> usize {
        let at = (offset as usize).min(self.data.len());
        let count = out.len().min(self.data.len() - at).min(4096);
        out[..count].copy_from_slice(&self.data[at..at + count]);
        self.read.set(self.read.get() + count as u64);
        count
    }
}

fn file(name: &str) -> Vec<u8> {
    std::fs::read(format!("{MEDIA}/{name}")).unwrap_or_else(|_| data(name))
}

fn frames_match(name: &str) {
    let data = file(name);
    let mut memory = VideoDecoder::open(data.clone()).unwrap();
    let source: Box<dyn Source> = Box::new(Counting {
        data,
        read: 0.into(),
    });
    let mut disk = VideoDecoder::from_source(source, DecoderLimits::default()).unwrap();
    assert_eq!(memory.frame_count(), disk.frame_count(), "{name}");
    assert_eq!(memory.duration_ms(), disk.duration_ms(), "{name}");
    assert_eq!(memory.info().width, disk.info().width, "{name}");
    let len = memory.info().rgba_len().unwrap();
    let (mut a, mut b) = (vec![0u8; len], vec![0u8; len]);
    let count = memory.frame_count();
    assert!(count > 0, "{name}");
    for index in 0..count {
        let shown_a = memory.decode(index, &mut a).unwrap();
        let shown_b = disk.decode(index, &mut b).unwrap();
        assert_eq!(shown_a.pts_ms, shown_b.pts_ms, "{name} frame {index}");
        assert!(a == b, "{name}: frame {index} differs");
    }
    // Backwards, which restarts from a key frame.
    let last = count - 1;
    for index in [last, 0, last / 2] {
        memory.decode(index, &mut a).unwrap();
        disk.decode(index, &mut b).unwrap();
        assert!(a == b, "{name}: frame {index} differs after a seek");
    }
}

#[test]
fn fragmented_mp4_through_a_source() {
    frames_match("fragmented.mp4");
}

#[test]
fn program_stream_through_a_source() {
    frames_match("mpeg1.mpg");
    frames_match("av-mp2.mpg");
}

/// A fragmented file's sound, from memory and from a file.
#[test]
fn fragmented_mp4_audio() {
    let bytes = file("av-frag.mp4");
    let whole = audio_track(&bytes).unwrap();
    assert!(whole.packets.len() > 40, "{}", whole.packets.len());
    let source = Counting { data: bytes.clone(), read: 0.into() };
    let (_, disk) = audio_track_from_source(&source).unwrap();
    assert_eq!(whole.packets.len(), disk.packets.len());
    for (a, b) in whole.packets.iter().zip(&disk.packets) {
        assert_eq!((a.offset, a.size, a.pts_ms), (b.offset, b.size, b.pts_ms));
    }
    frames_match("av-frag.mp4");
}

/// Packets as they'd come from the whole file, each read from where the file has it.
#[test]
fn program_stream_audio_through_a_source() {
    let name = "av-mp2.mpg";
    let bytes = file(name);
    let whole = audio_track(&bytes).unwrap();
    let source = Counting { data: bytes.clone(), read: 0.into() };
    let (_, disk) = audio_track_from_source(&source).unwrap();
    assert_eq!(whole.packets.len(), disk.packets.len());
    for (a, b) in whole.packets.iter().zip(&disk.packets) {
        assert_eq!((a.offset, a.size, a.pts_ms), (b.offset, b.size, b.pts_ms));
    }
}

#[test]
fn mp4_through_a_source() {
    frames_match("h264-sample.mp4");
    frames_match("av-sample.mp4");
}

#[test]
fn matroska_through_a_source() {
    frames_match("h264-sample.mkv");
}

#[test]
fn flv_through_a_source() {
    frames_match("h264-sample.flv");
}

#[test]
fn avi_through_a_source() {
    frames_match("motion-jpeg.avi");
    frames_match("raw-video.avi");
}

#[test]
fn a_sample_is_read_when_it_is_decoded() {
    let data = file("av-sample.mp4");
    let size = data.len() as u64;
    let source = Box::new(Counting {
        data,
        read: 0.into(),
    });
    let counter: *const Counting = &*source;
    let mut disk = VideoDecoder::from_source(source, DecoderLimits::default()).unwrap();
    // SAFETY: the box it points into lives in `disk` until the end of this test.
    let read_after_open = unsafe { (*counter).read.get() };
    assert!(read_after_open < size, "opening read {read_after_open} of {size}");
    let mut frame = vec![0u8; disk.info().rgba_len().unwrap()];
    disk.decode(0, &mut frame).unwrap();
}

#[test]
fn audio_tracks_match() {
    for name in ["av-sample.mp4"] {
        let data = file(name);
        let memory = audio_track(&data).unwrap();
        let source = data.as_slice();
        let (head, disk) = audio_track_from_source(&source).unwrap();
        assert_eq!(memory.codec, disk.codec);
        assert_eq!(memory.packets, disk.packets);
        assert_eq!(head[disk.config.clone()], data[memory.config.clone()]);
        let mut packet = Vec::new();
        assert!(disk.read_packet(&source, 3, &mut packet));
        assert_eq!(packet, memory.packet(&data, 3).unwrap());
    }
}

fn data(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn audio_tracks_in_the_other_containers() {
    for name in ["av-aac.flv", "av-aac.mp4", "av-flac.mkv", "av-mp3.mkv", "av-pcm.avi", "av-pcm.mov", "av-vorbis.mkv"] {
        let data = data(name);
        let memory = audio_track(&data).unwrap();
        let source = data.as_slice();
        let (head, disk) = audio_track_from_source(&source).unwrap();
        assert_eq!(memory.codec, disk.codec, "{name}");
        assert_eq!(memory.packets, disk.packets, "{name}");
        assert_eq!(head[disk.config.clone()], data[memory.config.clone()], "{name}");
    }
}

#[test]
fn b_frames_through_a_source() {
    for name in ["bframes.avi", "bframes.flv", "bframes.mkv", "bframes.mp4"] {
        let data = data(name);
        let mut memory = VideoDecoder::open(data.clone()).unwrap();
        let source: Box<dyn Source> = Box::new(Counting {
            data,
            read: 0.into(),
        });
        let mut disk = VideoDecoder::from_source(source, DecoderLimits::default()).unwrap();
        assert_eq!(memory.frame_count(), disk.frame_count(), "{name}");
        let len = memory.info().rgba_len().unwrap();
        let (mut a, mut b) = (vec![0u8; len], vec![0u8; len]);
        for index in 0..memory.frame_count() {
            let x = memory.decode(index, &mut a).unwrap();
            let y = disk.decode(index, &mut b).unwrap();
            assert_eq!(x.pts_ms, y.pts_ms, "{name} {index}");
            assert!(a == b, "{name}: frame {index}");
        }
    }
}
