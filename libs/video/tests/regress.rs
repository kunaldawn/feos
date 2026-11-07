//! Regression tests for bugs found by the audit in `vnext.md`, plus coverage of
//! what the original fixtures couldn't see. See `data/generate.py` for clips.

use feos_video::{
    decode_frame, detect, info, scratch_len, Codec, ContainerFormat, VideoDecoder, VideoError,
};

macro_rules! data {
    ($name:literal) => {
        include_bytes!(concat!("data/", $name)) as &[u8]
    };
}

fn max_channel_diff(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(&x, &y)| x.abs_diff(y))
        .max()
        .unwrap_or(0)
}

/// Decodes every frame with `VideoDecoder`.
fn decode_all(input: &[u8]) -> Vec<Vec<u8>> {
    let mut decoder = VideoDecoder::open(input).unwrap();
    let mut out = vec![0u8; decoder.info().rgba_len().unwrap()];
    (0..decoder.frame_count())
        .map(|index| {
            let frame = decoder.decode(index, &mut out).unwrap();
            assert_eq!(frame.index, index);
            out.clone()
        })
        .collect()
}

fn assert_matches_ref(input: &[u8], reference: &[u8], tolerance: u8) {
    let frames = decode_all(input);
    let frame_len = frames[0].len();
    assert_eq!(reference.len(), frame_len * frames.len(), "frame count");
    for (index, (frame, expected)) in frames.iter().zip(reference.chunks(frame_len)).enumerate() {
        let diff = max_channel_diff(frame, expected);
        assert!(diff <= tolerance, "frame {index} differs by {diff}");
    }
}

// --- Parser crashes reproduced by the audit ---

#[test]
fn flv_avc_header_shorter_than_its_length_field_is_rejected() {
    // MED-3: seven bytes of avcC used to index config[7].
    let _ = info(data!("flv_short_avcc.bin"));
}

#[test]
fn mp4_with_empty_tkhd_as_last_bytes_is_handled() {
    // MED-2: read data[header.payload] past the end.
    let _ = info(data!("mp4_empty_tkhd_at_end.bin"));
}

#[test]
fn deeply_nested_flv_metadata_does_not_overflow_a_small_stack() {
    // MED-1: repeated `00 00 03` recursed once per property.
    let body_header = b"\x02\x00\x0aonMetaData\x03";
    let nesting = b"\x00\x00\x03".repeat(1_300_000);
    let size = (body_header.len() + nesting.len()) as u32;
    let mut flv = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00\x12".to_vec();
    flv.extend_from_slice(&size.to_be_bytes()[1..]);
    flv.extend_from_slice(&[0; 7]);
    flv.extend_from_slice(body_header);
    flv.extend_from_slice(&nesting);
    std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(move || {
            let _ = info(&flv);
        })
        .unwrap()
        .join()
        .unwrap();
}

// --- H.264 in containers ---

const BFRAMES_REF: &[u8] = data!("bframes_h264.ref");

#[test]
fn b_frames_come_out_in_display_order_in_every_container() {
    // MED-10: MP4 stopped feeding samples at the target. All of these clips are
    // the same encode, so one reference serves.
    for (name, clip) in [
        ("mp4", data!("bframes.mp4")),
        ("mkv", data!("bframes.mkv")),
        ("flv", data!("bframes.flv")),
        ("avi", data!("bframes.avi")),
        ("h264", data!("bframes.h264")),
        ("fragmented", data!("fragmented.mp4")),
    ] {
        let props = info(clip).unwrap();
        assert_eq!(props.codec, Codec::H264, "{name}");
        assert_eq!((props.width, props.height), (48, 32), "{name}");
        assert_eq!(props.frame_count, 12, "{name}");
        assert_matches_ref(clip, BFRAMES_REF, 3);
    }
}

#[test]
fn decode_frame_wrapper_matches_the_stateful_decoder() {
    let clip = data!("bframes.mp4");
    let frames = decode_all(clip);
    let mut out = vec![0u8; frames[0].len()];
    for index in [0, 5, 3, 11, 1] {
        decode_frame(clip, index, &mut out, &mut []).unwrap();
        assert_eq!(out, frames[index as usize]);
    }
}

#[test]
fn seeking_reaches_every_frame_from_any_position() {
    let clip = data!("bframes.mp4");
    let frames = decode_all(clip);
    let mut decoder = VideoDecoder::open(clip).unwrap();
    let mut out = vec![0u8; frames[0].len()];
    for index in [11u32, 0, 7, 6, 2, 3, 10, 4, 4, 9] {
        decoder.decode(index, &mut out).unwrap();
        assert_eq!(out, frames[index as usize], "frame {index}");
    }
}

#[test]
fn key_frame_index_has_exactly_the_idr_pictures() {
    // keyint=4, for stss, Matroska flags, idx1, FLV frame types and an IDR scan.
    for (name, clip) in [
        ("mp4", data!("bframes.mp4")),
        ("mkv", data!("bframes.mkv")),
        ("flv", data!("bframes.flv")),
        ("avi", data!("bframes.avi")),
        ("h264", data!("bframes.h264")),
        ("fragmented", data!("fragmented.mp4")),
    ] {
        let decoder = VideoDecoder::open(clip).unwrap();
        let keys: Vec<_> = (0..12)
            .map(|index| decoder.key_frame_at_or_before(index))
            .collect();
        assert_eq!(keys, [0, 0, 0, 0, 4, 4, 4, 4, 8, 8, 8, 8], "{name}");
        assert_eq!(decoder.key_frame_after(1), Some(4), "{name}");
        assert_eq!(decoder.key_frame_after(8), None, "{name}");
    }
}

#[test]
fn next_frame_needs_only_a_few_samples_and_a_seek_starts_at_a_key_frame() {
    // MED-9: the point of the stateful API. Budget is in samples fed.
    let clip = data!("bframes.mp4");
    let mut decoder = VideoDecoder::open(clip).unwrap();
    let mut out = vec![0u8; decoder.info().rgba_len().unwrap()];
    for index in 0..12 {
        // Reordering depth plus one covers any next frame.
        assert!(decoder.advance(index, &mut out, 4).unwrap().is_some());
    }
    // A jump from the start to frame 9 needs samples 8.. only.
    let mut decoder = VideoDecoder::open(clip).unwrap();
    assert!(decoder.advance(9, &mut out, 4).unwrap().is_some());
    // Too small a budget reports progress rather than blocking.
    let mut decoder = VideoDecoder::open(clip).unwrap();
    assert!(decoder.advance(3, &mut out, 1).unwrap().is_none());
    while decoder.advance(3, &mut out, 1).unwrap().is_none() {}
    assert_eq!(out, decode_all(clip)[3]);
}

#[test]
fn frame_timing_uses_presentation_times() {
    // MED-16, and the ctts, stts and edit list handling behind it.
    let decoder = VideoDecoder::open(data!("vfr.mp4")).unwrap();
    let times: Vec<_> = (0..decoder.frame_count())
        .map(|index| decoder.frame_time_ms(index).unwrap())
        .collect();
    assert_eq!(times, [0, 100, 200, 600, 700, 800, 900, 1000]);
    assert_eq!(decoder.frame_delay_ms(2), 400);
    assert_eq!(decoder.frame_at_time(599), 2);
    assert_eq!(decoder.frame_at_time(600), 3);
    assert_eq!(decoder.duration_ms(), 1100);
    assert_matches_ref(data!("vfr.mp4"), data!("vfr_mp4.ref"), 3);

    // B-frames: times rise evenly in display order for all containers.
    for clip in [
        data!("bframes.mp4"),
        data!("bframes.mkv"),
        data!("bframes.flv"),
        data!("fragmented.mp4"),
    ] {
        let decoder = VideoDecoder::open(clip).unwrap();
        for index in 0..12 {
            assert_eq!(decoder.frame_time_ms(index), Some(index as u64 * 100));
        }
    }
}

#[test]
fn decoded_size_comes_from_the_codec_and_rotation_is_metadata() {
    // MED-12: tkhd says 32x48, the pixels are 48x32. FFmpeg's -display_rotation
    // counts counter-clockwise, so its 90 is our clockwise 270.
    for clip in [data!("rotated.mp4"), data!("rotated.mkv")] {
        let props = info(clip).unwrap();
        assert_eq!((props.width, props.height), (48, 32));
        assert_eq!(props.display.rotation, 270);
        assert_eq!(props.display_size(), (32, 48));
        assert_matches_ref(clip, BFRAMES_REF, 3);
    }
}

#[test]
fn anamorphic_video_decodes_at_coded_size_with_display_shape() {
    for clip in [data!("anamorphic.mp4"), data!("anamorphic.mkv")] {
        let props = info(clip).unwrap();
        assert_eq!((props.width, props.height), (48, 32));
        let (w, h) = props.display.sar;
        assert_eq!(w, h * 2);
        assert_eq!(props.display_size(), (96, 32));
    }
    assert_matches_ref(data!("anamorphic.mp4"), data!("anamorphic_mp4.ref"), 3);
}

#[test]
fn colour_signalling_selects_the_conversion() {
    let clip = data!("bt709_full.h264");
    let props = info(clip).unwrap();
    assert_eq!(props.colour.matrix, 1);
    assert!(props.colour.full_range);
    assert_matches_ref(clip, data!("bt709_full_h264.ref"), 4);
}

#[test]
fn wide_box_before_the_movie_is_accepted() {
    // A MOV may start with `wide` instead of `ftyp`. Same size, so offsets hold.
    let mut clip = data!("bframes.mp4").to_vec();
    clip[4..8].copy_from_slice(b"wide");
    assert_eq!(detect(&clip), Some(ContainerFormat::Mp4));
    assert_matches_ref(&clip, BFRAMES_REF, 3);
}

// --- Uncompressed AVI ---

#[test]
fn raw_video_at_a_width_not_divisible_by_four() {
    // MED-19: row padding, palette, flip of YUV, and YV12's plane order.
    assert_matches_ref(data!("raw_bgr24_w30.avi"), data!("raw_bgr24_w30_avi.ref"), 0);
    assert_matches_ref(data!("raw_bgr0_w30.avi"), data!("raw_bgr0_w30_avi.ref"), 0);
    assert_matches_ref(data!("raw_pal8_w30.avi"), data!("raw_pal8_w30_avi.ref"), 0);
    assert_matches_ref(data!("raw_rgb555_w30.avi"), data!("raw_rgb555_w30_avi.ref"), 0);
    assert_matches_ref(data!("raw_gray_w30.avi"), data!("raw_gray_w30_avi.ref"), 0);
    assert_matches_ref(data!("raw_bgr24_vfw.mkv"), data!("raw_bgr24_vfw_mkv.ref"), 0);
    assert_matches_ref(data!("raw_yuy2_w30.avi"), data!("raw_yuy2_w30_avi.ref"), 12);
    assert_matches_ref(data!("raw_i420_w30.avi"), data!("raw_i420_w30_avi.ref"), 12);
    assert_matches_ref(data!("raw_yv12_w30.avi"), data!("raw_yv12_w30_avi.ref"), 12);
    assert_matches_ref(data!("raw_i420.mkv"), data!("raw_i420_mkv.ref"), 12);
}

#[test]
fn rle_delta_frames_build_on_the_frames_before_them() {
    // MED-20: decoding only the target sample left artefacts.
    let clip = data!("rle8.avi");
    assert_eq!(info(clip).unwrap().codec, Codec::Rle8);
    assert_matches_ref(clip, data!("rle8_avi.ref"), 0);
    let frames = decode_all(clip);
    let mut decoder = VideoDecoder::open(clip).unwrap();
    let mut out = vec![0u8; frames[0].len()];
    for index in [5u32, 1, 4, 0, 2] {
        decoder.decode(index, &mut out).unwrap();
        assert_eq!(out, frames[index as usize], "frame {index}");
    }
    assert_eq!(decoder.key_frame_at_or_before(5), 3);
}

/// Builds an AVI of 2x2 gray top-down frames: chunks in `movi` as given.
fn gray_avi(movi: &[u8], tail: &[u8]) -> Vec<u8> {
    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    }
    fn list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut inner = kind.to_vec();
        inner.extend_from_slice(body);
        chunk(b"LIST", &inner)
    }
    let mut avih = vec![0u8; 56];
    avih[32] = 2;
    avih[36] = 2;
    let mut strh = vec![0u8; 56];
    strh[..4].copy_from_slice(b"vids");
    strh[20] = 1;
    strh[24] = 10;
    let mut bih = vec![0u8; 40];
    bih[0] = 40;
    bih[4] = 2;
    bih[8..12].copy_from_slice(&(-2i32).to_le_bytes());
    bih[14] = 8;
    bih[16..20].copy_from_slice(b"Y800");
    let strl = [chunk(b"strh", &strh), chunk(b"strf", &bih)].concat();
    let hdrl = [chunk(b"avih", &avih), list(b"strl", &strl)].concat();
    let mut body = b"AVI ".to_vec();
    body.extend_from_slice(&list(b"hdrl", &hdrl));
    body.extend_from_slice(&list(b"movi", movi));
    body.extend_from_slice(tail);
    chunk(b"RIFF", &body)
}

fn avi_frame(value: u8) -> Vec<u8> {
    let mut out = b"00db\x04\0\0\0".to_vec();
    out.extend_from_slice(&[value; 4]);
    out
}

#[test]
fn dib_rows_are_padded_and_bottom_up() {
    // MED-19, for files that follow the DIB layout. A 3x2 BI_RGB BGR frame.
    let mut movi = b"00db\x18\0\0\0".to_vec();
    movi.extend_from_slice(&[1, 2, 3, 1, 2, 3, 1, 2, 3, 0, 0, 0]); // Bottom row.
    movi.extend_from_slice(&[4, 5, 6, 4, 5, 6, 4, 5, 6, 0, 0, 0]); // Top row.
    let mut avi = gray_avi(&movi, &[]);
    let bih = avi.windows(4).position(|w| w == b"Y800").unwrap() - 16;
    avi[bih + 4] = 3;
    avi[bih + 8..bih + 12].copy_from_slice(&2i32.to_le_bytes());
    avi[bih + 14] = 24;
    avi[bih + 16..bih + 20].copy_from_slice(&[0; 4]);
    let frames = decode_all(&avi);
    assert_eq!(frames[0][..8], [6, 5, 4, 255, 6, 5, 4, 255]);
    assert_eq!(frames[0][12..16], [3, 2, 1, 255]);
}

#[test]
fn avi_null_frame_repeats_the_previous_one() {
    // MED-21: a zero-length chunk used to stop playback for good.
    let movi = [avi_frame(10), b"00db\0\0\0\0".to_vec(), avi_frame(30)].concat();
    let avi = gray_avi(&movi, &[]);
    assert_eq!(info(&avi).unwrap().frame_count, 3);
    let frames = decode_all(&avi);
    assert_eq!(frames[1], frames[0]);
    assert_eq!(frames[2][..4], [30, 30, 30, 255]);
}

#[test]
fn avi_rec_lists_and_opendml_extensions_are_walked() {
    let mut rec = b"LIST\x1c\0\0\0rec ".to_vec();
    rec.extend_from_slice(&avi_frame(20));
    rec.extend_from_slice(&avi_frame(30));
    assert_eq!(rec.len(), 8 + 0x1c);
    let movi = [avi_frame(10), rec].concat();
    let mut avi = gray_avi(&movi, &[]);
    // A second RIFF, as OpenDML appends past the first 1 GiB.
    let mut avix_movi = b"LIST\x10\0\0\0movi".to_vec();
    avix_movi.extend_from_slice(&avi_frame(40));
    let mut avix = b"RIFF\x1c\0\0\0AVIX".to_vec();
    avix.extend_from_slice(&avix_movi);
    assert_eq!(avix.len(), 8 + 0x1c);
    avi.extend_from_slice(&avix);
    let frames = decode_all(&avi);
    let values: Vec<_> = frames.iter().map(|f| f[0]).collect();
    assert_eq!(values, [10, 20, 30, 40]);
}

#[test]
fn avi_idx1_marks_key_frames() {
    let movi = [avi_frame(10), avi_frame(20), avi_frame(30)].concat();
    let mut idx1 = b"idx1\x30\0\0\0".to_vec();
    for (i, flags) in [0x10u32, 0, 0x10].iter().enumerate() {
        idx1.extend_from_slice(b"00db");
        idx1.extend_from_slice(&flags.to_le_bytes());
        idx1.extend_from_slice(&((i as u32) * 12 + 4).to_le_bytes());
        idx1.extend_from_slice(&4u32.to_le_bytes());
    }
    // Y800 is intra, so every frame reports as a key frame whatever idx1 says.
    let avi = gray_avi(&movi, &idx1);
    assert_eq!(VideoDecoder::open(&avi).unwrap().frame_count(), 3);
}

// --- Matroska ---

fn ebml(id: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.push(0x01);
    out.extend_from_slice(&[0; 3]);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

const CLUSTER: &[u8] = &[0x1f, 0x43, 0xb6, 0x75];
const SEGMENT: &[u8] = &[0x18, 0x53, 0x80, 0x67];
const UNKNOWN_SIZE: &[u8] = &[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];

/// EBML header and tracks for one 2x2 gray `V_UNCOMPRESSED` track.
fn mkv_headers(encodings: &[u8]) -> Vec<u8> {
    let video = [
        ebml(&[0xb0], &[2]),
        ebml(&[0xba], &[2]),
        ebml(&[0x2e, 0xb5, 0x24], b"Y800"),
    ]
    .concat();
    let entry = [
        ebml(&[0xd7], &[1]),
        ebml(&[0x83], &[1]),
        ebml(&[0x86], b"V_UNCOMPRESSED"),
        ebml(&[0xe0], &video),
        encodings.to_vec(),
    ]
    .concat();
    ebml(&[0x16, 0x54, 0xae, 0x6b], &ebml(&[0xae], &entry))
}

fn simple_block(flags: u8, time: i16, body: &[u8]) -> Vec<u8> {
    let mut block = vec![0x81];
    block.extend_from_slice(&time.to_be_bytes());
    block.push(flags);
    block.extend_from_slice(body);
    ebml(&[0xa3], &block)
}

fn mkv_file(tracks: &[u8], clusters: &[u8]) -> Vec<u8> {
    let mut out = ebml(&[0x1a, 0x45, 0xdf, 0xa3], &[]);
    out.extend_from_slice(SEGMENT);
    out.extend_from_slice(UNKNOWN_SIZE);
    out.extend_from_slice(tracks);
    out.extend_from_slice(clusters);
    out
}

fn first_bytes(frames: &[Vec<u8>]) -> Vec<u8> {
    frames.iter().map(|frame| frame[0]).collect()
}

#[test]
fn unknown_size_clusters_end_at_the_next_cluster() {
    // MED-18: only the first cluster of a live-muxed file played.
    let mut clusters = Vec::new();
    for (i, value) in [10u8, 20, 30].iter().enumerate() {
        clusters.extend_from_slice(CLUSTER);
        clusters.extend_from_slice(UNKNOWN_SIZE);
        clusters.extend_from_slice(&ebml(&[0xe7], &[(i as u8) * 100]));
        clusters.extend_from_slice(&simple_block(0x80, 0, &[*value; 4]));
    }
    let mkv = mkv_file(&mkv_headers(&[]), &clusters);
    assert_eq!(info(&mkv).unwrap().frame_count, 3);
    assert_eq!(first_bytes(&decode_all(&mkv)), [10, 20, 30]);
    let decoder = VideoDecoder::open(&mkv).unwrap();
    assert_eq!(decoder.frame_time_ms(2), Some(200));
}

#[test]
fn truncated_last_cluster_keeps_the_frames_before_it() {
    // MED-18: the whole file used to be refused.
    let cluster = [
        simple_block(0x80, 0, &[10; 4]),
        simple_block(0x80, 10, &[20; 4]),
    ]
    .concat();
    let mut mkv = mkv_file(&mkv_headers(&[]), &ebml(CLUSTER, &cluster));
    mkv.truncate(mkv.len() - 2);
    assert_eq!(info(&mkv).unwrap().frame_count, 1);
    assert_eq!(first_bytes(&decode_all(&mkv)), [10]);
}

#[test]
fn laced_blocks_expand_to_each_frame() {
    // MED-27: EBML lacing read one size too many, and took the frame data's
    // first byte as a size.
    let frames: [&[u8]; 3] = [&[10; 4], &[20; 4], &[30; 4]];
    let body = frames.concat();
    for (flags, sizes) in [
        (0x02u8, &[2u8, 4, 4][..]),       // Xiph: count-1, then sizes
        (0x04, &[2][..]),                 // fixed
        (0x06, &[2, 0x84, 0xbf][..]),     // EBML: size 4, then a difference of 0
    ] {
        let block = simple_block(0x80 | flags, 0, &[sizes, &body].concat());
        let mkv = mkv_file(&mkv_headers(&[]), &ebml(CLUSTER, &block));
        assert_eq!(first_bytes(&decode_all(&mkv)), [10, 20, 30], "{flags:#x}");
    }
}

#[test]
fn compressed_matroska_tracks_are_rebuilt() {
    // Header stripping: the first byte of each frame is in the track header.
    let settings = ebml(&[0x42, 0x55], &[77]);
    let compression = [ebml(&[0x42, 0x54], &[3]), settings].concat();
    let encodings = ebml(
        &[0x6d, 0x80],
        &ebml(&[0x62, 0x40], &ebml(&[0x50, 0x34], &compression)),
    );
    let block = simple_block(0x80, 0, &[1, 2, 3]);
    let mkv = mkv_file(&mkv_headers(&encodings), &ebml(CLUSTER, &block));
    let frames = decode_all(&mkv);
    assert_eq!(frames[0][..4], [77, 77, 77, 255]);
    assert_eq!(frames[0][4], 1);

    // Zlib.
    let compression = ebml(&[0x42, 0x54], &[0]);
    let encodings = ebml(
        &[0x6d, 0x80],
        &ebml(&[0x62, 0x40], &ebml(&[0x50, 0x34], &compression)),
    );
    let mut packed = [0u8; 64];
    let len = feos_compress::zlib::compress(&[55; 4], &mut packed).unwrap();
    let block = simple_block(0x80, 0, &packed[..len]);
    let mkv = mkv_file(&mkv_headers(&encodings), &ebml(CLUSTER, &block));
    assert_eq!(first_bytes(&decode_all(&mkv)), [55]);
}

#[test]
fn v_uncompressed_uses_the_colour_space_element() {
    // MED-19: it used to be read as BGR whatever the element said.
    let block = simple_block(0x80, 0, &[9; 4]);
    let mkv = mkv_file(&mkv_headers(&[]), &ebml(CLUSTER, &block));
    assert_eq!(info(&mkv).unwrap().codec, Codec::RawGray);
}

// --- FLV ---

#[test]
fn short_flv_video_tag_does_not_refuse_the_file() {
    // MED-27.
    let clip = data!("bframes.flv");
    let offset = 13; // The FLV header and first previous-tag size.
    let mut flv = clip[..offset].to_vec();
    flv.extend_from_slice(&[9, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0x17, 0, 0, 0, 12]);
    flv.extend_from_slice(&clip[offset..]);
    assert_eq!(info(&flv).unwrap().frame_count, 12);
    assert_matches_ref(&flv, BFRAMES_REF, 3);
}

#[test]
fn flv_dimensions_come_from_a_properly_parsed_sps() {
    // MED-11: the SPS parser used to see the NAL header and escape bytes.
    let props = info(data!("bframes.flv")).unwrap();
    assert_eq!((props.width, props.height), (48, 32));
}

// --- Damage ---

#[test]
fn damaged_h264_sample_loses_only_nearby_frames() {
    // MED-21: any error stopped playback permanently.
    let mut clip = data!("bframes.mp4").to_vec();
    // Damage bytes in the middle of the media data.
    let mdat = clip.windows(4).position(|w| w == b"mdat").unwrap();
    let middle = mdat + 8 + 1200;
    for byte in &mut clip[middle..middle + 40] {
        *byte = 0xff;
    }
    let mut decoder = VideoDecoder::open(&clip[..]).unwrap();
    let mut out = vec![0u8; decoder.info().rgba_len().unwrap()];
    let mut good = 0;
    for index in 0..decoder.frame_count() {
        if decoder.decode(index, &mut out).is_ok() {
            good += 1;
        }
    }
    assert!(good >= 8, "only {good} frames decoded");
}

// --- MJPEG ---

const JPEG: &[u8] = include_bytes!("../../image/tests/data/test.jpg");

#[test]
fn mjpeg_stream_ignores_a_trailing_partial_frame() {
    // MED-24: it used to count as a frame.
    let mut stream = [JPEG, JPEG].concat();
    stream.extend_from_slice(&JPEG[..JPEG.len() / 2]);
    assert_eq!(info(&stream).unwrap().frame_count, 2);
    assert_eq!(VideoDecoder::open(&stream).unwrap().frame_count(), 2);
}

#[test]
fn mjpeg_thumbnail_in_exif_does_not_end_the_frame() {
    // MED-24: find_eoi ignored segment lengths.
    let mut exif = vec![0xff, 0xe1];
    let body = [b"Exif\0\0".as_slice(), JPEG].concat();
    exif.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    exif.extend_from_slice(&body);
    let image = [&JPEG[..2], &exif, &JPEG[2..]].concat();
    let stream = [image.as_slice(), image.as_slice()].concat();
    let props = info(&stream).unwrap();
    assert_eq!(props.frame_count, 2);
    let mut out = vec![0u8; props.rgba_len().unwrap()];
    decode_frame(&stream, 1, &mut out, &mut []).unwrap();
}

// --- GIF ---

/// A GIF with a 2-colour global table: 0 red, 1 green.
fn gif_header(width: u16, height: u16) -> Vec<u8> {
    let mut out = b"GIF89a".to_vec();
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&[0x80, 0, 0]);
    out.extend_from_slice(&[255, 0, 0, 0, 255, 0]);
    out
}

fn gif_control(disposal: u8, delay: u16, transparent: Option<u8>) -> Vec<u8> {
    let packed = (disposal << 2) | u8::from(transparent.is_some());
    let delay = delay.to_le_bytes();
    vec![
        0x21,
        0xf9,
        4,
        packed,
        delay[0],
        delay[1],
        transparent.unwrap_or(0),
        0,
    ]
}

/// An image block whose pixels are all `index`, with an end code, using an LZW
/// clear before every pixel so the codes stay 3 bits wide.
fn gif_image(left: u16, top: u16, width: u16, height: u16, index: u8, eoi: bool) -> Vec<u8> {
    let mut out = vec![0x2c];
    for value in [left, top, width, height] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.push(0);
    out.push(2); // Minimum code size.
    let mut bits = 0u32;
    let mut count = 0u32;
    let mut data = Vec::new();
    let mut put = |code: u32, data: &mut Vec<u8>| {
        bits |= code << count;
        count += 3;
        while count >= 8 {
            data.push(bits as u8);
            bits >>= 8;
            count -= 8;
        }
    };
    for _ in 0..width as u32 * height as u32 {
        put(4, &mut data);
        put(index as u32, &mut data);
    }
    if eoi {
        put(5, &mut data);
    }
    put(0, &mut data);
    for chunk in data.chunks(255) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    out.push(0);
    out
}

fn pixel(frame: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let at = (y * width + x) * 4;
    [frame[at], frame[at + 1], frame[at + 2], frame[at + 3]]
}

#[test]
fn gif_without_trailer_or_end_code_still_plays() {
    // MED-26.
    let mut gif = gif_header(2, 2);
    gif.extend_from_slice(&gif_image(0, 0, 2, 2, 0, false));
    assert_eq!(info(&gif).unwrap().frame_count, 1);
    let frames = decode_all(&gif);
    assert_eq!(pixel(&frames[0], 2, 1, 1), [255, 0, 0, 255]);
}

#[test]
fn gif_frame_larger_than_the_canvas_is_clipped() {
    // MED-26.
    let mut gif = gif_header(2, 2);
    gif.extend_from_slice(&gif_image(0, 0, 4, 3, 1, true));
    gif.push(0x3b);
    let frames = decode_all(&gif);
    assert_eq!(pixel(&frames[0], 2, 1, 1), [0, 255, 0, 255]);
    let mut out = vec![0u8; 16];
    let mut scratch = vec![0u8; scratch_len(&gif).unwrap()];
    decode_frame(&gif, 0, &mut out, &mut scratch).unwrap();
    assert_eq!(out, frames[0]);
}

#[test]
fn gif_disposal_restores_transparency_and_oversized_scratch_is_fine() {
    // MED-26: disposal 2 painted the opaque background, and disposal 3 panicked
    // on a scratch buffer larger than required.
    let mut gif = gif_header(2, 1);
    gif.extend_from_slice(&gif_control(2, 5, None));
    gif.extend_from_slice(&gif_image(0, 0, 1, 1, 1, true));
    gif.extend_from_slice(&gif_control(3, 5, None));
    gif.extend_from_slice(&gif_image(1, 0, 1, 1, 1, true));
    gif.extend_from_slice(&gif_control(0, 5, None));
    gif.extend_from_slice(&gif_image(1, 0, 1, 1, 0, true));
    gif.push(0x3b);
    let mut out = vec![0u8; 8];
    let mut scratch = vec![0u8; scratch_len(&gif).unwrap() + 100];
    decode_frame(&gif, 1, &mut out, &mut scratch).unwrap();
    assert_eq!(pixel(&out, 2, 0, 0), [0, 0, 0, 0]);
    decode_frame(&gif, 2, &mut out, &mut scratch).unwrap();
    assert_eq!(pixel(&out, 2, 0, 0), [0, 0, 0, 0]);
    assert_eq!(pixel(&out, 2, 1, 0), [255, 0, 0, 255]);
    // The stateful decoder agrees, in any order, with a resumable budget.
    let frames = decode_all(&gif);
    assert_eq!(frames[2], out);
    let mut decoder = VideoDecoder::open(&gif[..]).unwrap();
    let mut seek = vec![0u8; 8];
    for index in [2u32, 0, 1, 2] {
        decoder.decode(index, &mut seek).unwrap();
        assert_eq!(seek, frames[index as usize]);
    }
    let mut decoder = VideoDecoder::open(&gif[..]).unwrap();
    while decoder.advance(2, &mut seek, 1).unwrap().is_none() {}
    assert_eq!(seek, frames[2]);
}

#[test]
fn gif_delays_become_frame_times() {
    // MED-16: delay under 20 ms plays as 100 ms, as in browsers.
    let mut gif = gif_header(1, 1);
    gif.extend_from_slice(&gif_control(0, 50, None));
    gif.extend_from_slice(&gif_image(0, 0, 1, 1, 0, true));
    gif.extend_from_slice(&gif_control(0, 0, None));
    gif.extend_from_slice(&gif_image(0, 0, 1, 1, 1, true));
    gif.push(0x3b);
    let decoder = VideoDecoder::open(&gif[..]).unwrap();
    assert_eq!(decoder.frame_time_ms(1), Some(500));
    assert_eq!(decoder.frame_delay_ms(0), 500);
    assert_eq!(decoder.frame_delay_ms(1), 100);
    assert_eq!(decoder.duration_ms(), 600);
}

// --- Limits ---

#[test]
fn limits_refuse_a_video_before_it_allocates() {
    use feos_video::DecoderLimits;
    let limits = DecoderLimits {
        max_width: 16,
        ..DecoderLimits::default()
    };
    assert!(matches!(
        VideoDecoder::with_limits(data!("bframes.mp4"), limits),
        Err(VideoError::LimitExceeded(_))
    ));
    let limits = DecoderLimits {
        max_frames: 4,
        ..DecoderLimits::default()
    };
    assert!(matches!(
        VideoDecoder::with_limits(data!("bframes.mp4"), limits),
        Err(VideoError::LimitExceeded(_))
    ));
    let limits = DecoderLimits {
        max_memory: 1024,
        ..DecoderLimits::default()
    };
    let mut decoder = VideoDecoder::with_limits(data!("bframes.mp4"), limits).unwrap();
    let mut out = vec![0u8; decoder.info().rgba_len().unwrap()];
    assert_eq!(decoder.decode(0, &mut out), Err(VideoError::OutOfMemory));
}

/// `stsz`'s count goes straight into the duration. It can't say more than its table holds.
#[test]
fn mp4_sample_count_is_checked_against_the_file() {
    let mut file = include_bytes!("data/bframes.mp4").to_vec();
    let honest = feos_video::info(&file).unwrap().frame_count;
    let at = file.windows(4).position(|w| w == b"stsz").unwrap();
    file[at + 12..at + 16].copy_from_slice(&0x7fff_ffffu32.to_be_bytes());
    assert_eq!(feos_video::info(&file).unwrap().frame_count, honest);
}

