use feos_image::{bmp, decode, detect, info, png, ppm, qoi, scratch_len, ImageError, ImageFormat};

const RGBA_PNG: &[u8] = include_bytes!("data/rgba.png");
const PALETTE_PNG: &[u8] = include_bytes!("data/palette.png");
const PALETTE_OPAQUE_PNG: &[u8] = include_bytes!("data/palette_opaque.png");
const INTERLACED_PNG: &[u8] = include_bytes!("data/interlaced.png");
const GRAY16_PNG: &[u8] = include_bytes!("data/gray16.png");
const GRAY_PNG: &[u8] = include_bytes!("data/gray.png");
const BMP: &[u8] = include_bytes!("data/test.bmp");
const BMP32: &[u8] = include_bytes!("data/test32.bmp");
const BMP_PALETTE: &[u8] = include_bytes!("data/palette.bmp");
const PPM: &[u8] = include_bytes!("data/test.ppm");
const JPEG: &[u8] = include_bytes!("data/test.jpg");
const JPEG420: &[u8] = include_bytes!("data/test420.jpg");
const GRAY_JPEG: &[u8] = include_bytes!("data/gray.jpg");

fn decode_image(input: &[u8]) -> (Vec<u8>, feos_image::ImageInfo) {
    let info = info(input).unwrap();
    let mut out = vec![0u8; info.rgba_len().unwrap()];
    let mut scratch = vec![0u8; scratch_len(input).unwrap()];
    let decoded = decode(input, &mut out, &mut scratch).unwrap();
    (out, decoded)
}

fn pixel(data: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let i = (y * width + x) * 4;
    [data[i], data[i + 1], data[i + 2], data[i + 3]]
}

#[test]
fn detects_every_format() {
    assert_eq!(detect(RGBA_PNG), Some(ImageFormat::Png));
    assert_eq!(detect(JPEG), Some(ImageFormat::Jpeg));
    assert_eq!(detect(BMP), Some(ImageFormat::Bmp));
    assert_eq!(detect(PPM), Some(ImageFormat::Ppm));
    assert_eq!(detect(b"qoif...."), Some(ImageFormat::Qoi));
    assert_eq!(detect(b"not an image"), None);
    assert_eq!(info(b"not an image"), Err(ImageError::UnknownFormat));
}

#[test]
fn decodes_rgba_png() {
    let (pixels, info) = decode_image(RGBA_PNG);
    assert_eq!((info.width, info.height), (16, 16));
    assert_eq!(pixel(&pixels, 16, 1, 1), [255, 0, 0, 255]);
    assert_eq!(pixel(&pixels, 16, 12, 1), [0, 255, 0, 255]);
    assert_eq!(pixel(&pixels, 16, 1, 12), [0, 0, 255, 255]);
    assert_eq!(pixel(&pixels, 16, 12, 12), [255, 255, 255, 128]);
}

#[test]
fn decodes_palette_and_transparency() {
    let (pixels, _) = decode_image(PALETTE_PNG);
    // Index 0 is red, index 1 green, index 2 blue with alpha 0.
    assert_eq!(pixel(&pixels, 16, 1, 1), [255, 0, 0, 255]);
    assert_eq!(pixel(&pixels, 16, 12, 1), [0, 255, 0, 255]);
    assert_eq!(pixel(&pixels, 16, 1, 12), [0, 0, 255, 0]);

    let (opaque, _) = decode_image(PALETTE_OPAQUE_PNG);
    assert_eq!(pixel(&opaque, 8, 2, 1)[3], 255);
}

#[test]
fn decodes_interlaced_png() {
    let (interlaced, _) = decode_image(INTERLACED_PNG);
    let (plain, _) = decode_image(RGBA_PNG);
    assert_eq!(interlaced, plain);
}

#[test]
fn decodes_gray_pngs() {
    let (gray16, info) = decode_image(GRAY16_PNG);
    assert_eq!((info.width, info.height), (8, 8));
    assert!(pixel(&gray16, 8, 0, 0)[0] <= 1);
    assert!(pixel(&gray16, 8, 7, 7)[0] >= 250);
    let (gray, _) = decode_image(GRAY_PNG);
    assert_eq!(pixel(&gray, 16, 0, 0)[0], pixel(&gray, 16, 0, 0)[1]);
}

#[test]
fn decodes_bmp_and_ppm() {
    let (bmp_pixels, info) = decode_image(BMP);
    assert_eq!((info.width, info.height), (16, 16));
    assert_eq!(pixel(&bmp_pixels, 16, 2, 2), [220, 20, 20, 255]);
    assert_eq!(pixel(&bmp_pixels, 16, 12, 12), [230, 230, 230, 255]);

    let (ppm_pixels, info) = decode_image(PPM);
    assert_eq!((info.width, info.height), (16, 16));
    assert_eq!(pixel(&ppm_pixels, 16, 2, 12), [20, 20, 220, 255]);

    let (bmp32, info) = decode_image(BMP32);
    assert_eq!((info.width, info.height), (4, 4));
    assert_eq!(pixel(&bmp32, 4, 0, 0)[3], 255);

    let (palette, info) = decode_image(BMP_PALETTE);
    assert_eq!((info.width, info.height), (4, 4));
    assert_eq!(pixel(&palette, 4, 0, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&palette, 4, 1, 0), [0, 255, 0, 255]);
    assert_eq!(pixel(&palette, 4, 2, 0), [0, 0, 255, 255]);
}

fn close(actual: [u8; 4], expected: [u8; 3], tolerance: i32) -> bool {
    (0..3).all(|i| (actual[i] as i32 - expected[i] as i32).abs() <= tolerance) && actual[3] == 255
}

#[test]
fn decodes_baseline_jpeg() {
    let (pixels, info) = decode_image(JPEG);
    assert_eq!((info.width, info.height), (16, 16));
    assert!(close(pixel(&pixels, 16, 3, 3), [220, 20, 20], 25));
    assert!(close(pixel(&pixels, 16, 12, 3), [20, 200, 20], 25));
    assert!(close(pixel(&pixels, 16, 3, 12), [20, 20, 220], 25));
    assert!(close(pixel(&pixels, 16, 12, 12), [230, 230, 230], 25));

    let (pixels, _) = decode_image(JPEG420);
    assert!(close(pixel(&pixels, 16, 3, 3), [220, 20, 20], 40));
    assert!(close(pixel(&pixels, 16, 12, 12), [230, 230, 230], 40));
}

#[test]
fn decodes_grayscale_jpeg() {
    let (pixels, info) = decode_image(GRAY_JPEG);
    assert_eq!((info.width, info.height), (8, 8));
    let value = pixel(&pixels, 8, 4, 4)[0];
    assert!((value as i32 - 128).abs() <= 2);
    assert_eq!(pixel(&pixels, 8, 4, 4)[0], pixel(&pixels, 8, 4, 4)[1]);
}

#[test]
fn png_roundtrip() {
    let mut rgba = vec![0u8; 7 * 5 * 4];
    for (i, pixel) in rgba.chunks_exact_mut(4).enumerate() {
        pixel[0] = (i * 7) as u8;
        pixel[1] = (i * 13) as u8;
        pixel[2] = (i * 29) as u8;
        pixel[3] = (i * 3) as u8;
    }
    let mut encoded = vec![0u8; 1024];
    let len = png::encode_rgba(7, 5, &rgba, &mut encoded).unwrap();
    encoded.truncate(len);

    let (decoded, info) = decode_image(&encoded);
    assert_eq!((info.width, info.height), (7, 5));
    assert_eq!(decoded, rgba);
}

#[test]
fn bmp_and_ppm_roundtrip() {
    let mut rgba = vec![0u8; 5 * 3 * 4];
    for (i, pixel) in rgba.chunks_exact_mut(4).enumerate() {
        pixel.copy_from_slice(&[(i * 11) as u8, (i * 17) as u8, (i * 23) as u8, 255]);
    }
    let mut encoded = vec![0u8; 2048];
    let len = bmp::encode_rgba(5, 3, &rgba, &mut encoded).unwrap();
    let (decoded, _) = decode_image(&encoded[..len]);
    assert_eq!(decoded, rgba);

    let len = ppm::encode_rgba(5, 3, &rgba, &mut encoded).unwrap();
    let (decoded, _) = decode_image(&encoded[..len]);
    assert_eq!(decoded, rgba);
}

#[test]
fn qoi_roundtrip() {
    let mut rgba = vec![0u8; 9 * 4 * 4];
    for (i, pixel) in rgba.chunks_exact_mut(4).enumerate() {
        pixel.copy_from_slice(&[(i * 5) as u8, (i * 9) as u8, (i * 3) as u8, 255]);
    }
    let mut encoded = vec![0u8; 4096];
    let len = qoi::encode_rgba(9, 4, &rgba, &mut encoded).unwrap();
    let (decoded, info) = decode_image(&encoded[..len]);
    assert_eq!((info.width, info.height), (9, 4));
    assert_eq!(decoded, rgba);
}

#[test]
fn reports_small_buffers() {
    let mut out = [0u8; 4];
    let mut scratch = [0u8; 4];
    assert!(matches!(
        decode(RGBA_PNG, &mut out, &mut scratch),
        Err(ImageError::BufferTooSmall { .. })
    ));
    let mut encoded = [0u8; 16];
    assert!(matches!(
        png::encode_rgba(16, 16, &[0u8; 1024], &mut encoded),
        Err(ImageError::BufferTooSmall { .. })
    ));
}

// ---------------------------------------------------------------- JPEG features

macro_rules! data {
    ($name:literal) => {
        include_bytes!(concat!("data/", $name)) as &[u8]
    };
}

/// Largest per-channel difference between RGBA pixels and raw RGB reference.
fn worst_against_rgb(rgba: &[u8], rgb: &[u8]) -> i32 {
    assert_eq!(rgba.len() / 4, rgb.len() / 3);
    rgba.chunks_exact(4)
        .zip(rgb.chunks_exact(3))
        .flat_map(|(a, b)| (0..3).map(move |i| (a[i] as i32 - b[i] as i32).abs()))
        .max()
        .unwrap()
}

#[track_caller]
fn assert_same_pixels(a: &[u8], b: &[u8]) {
    assert_eq!(decode_image(a).0, decode_image(b).0);
}

#[test]
fn jpeg_matches_libjpeg_where_no_upsampling_is_involved() {
    // The reference files are libjpeg-turbo's output (JDCT_ISLOW, the same
    // factorisation). Differences come from colour conversion rounding only.
    let (pixels, _) = decode_image(data!("baseline_444.jpg"));
    assert!(worst_against_rgb(&pixels, data!("baseline_444.rgb")) <= 1);
    let (pixels, _) = decode_image(data!("progressive_444.jpg"));
    assert!(worst_against_rgb(&pixels, data!("baseline_444.rgb")) <= 1);
}

#[test]
fn grayscale_jpeg_ignores_sampling_factors() {
    // Same coefficients, with SOF saying 2x2. This used to come out garbled.
    assert_same_pixels(data!("gray_h1v1.jpg"), data!("gray_h2v2.jpg"));
}

#[test]
fn jpeg_sampling_over_four_blocks_per_component() {
    let (reference, _) = decode_image(data!("interleaved_444.jpg"));
    // h*v = 8 and 16 on luma, with chroma at a quarter or less. Only smooth
    // regions can be compared with the 4:4:4 picture, since nearest-neighbour
    // chroma and box-filtered luma smear across the hard edge at x = 10..20.
    for fixture in [data!("sampling_h4v2.jpg"), data!("sampling_h4v4.jpg")] {
        let (pixels, info) = decode_image(fixture);
        assert_eq!((info.width, info.height), (37, 29));
        for (x, y) in [(2, 2), (30, 3), (3, 26), (32, 25)] {
            let want = pixel(&reference, 37, x, y);
            let got = pixel(&pixels, 37, x, y);
            assert!(
                close(got, [want[0], want[1], want[2]], 14),
                "{x},{y} {got:?} {want:?}"
            );
        }
    }
    // Doubled factors mean the same 1:1 ratio, at 12 blocks per MCU.
    assert_same_blurred(data!("sampling_doubled_444.jpg"), &reference);
}

/// Allows the small differences from re-coding the same picture at 2x2 blocks.
fn assert_same_blurred(fixture: &[u8], reference: &[u8]) {
    let (pixels, _) = decode_image(fixture);
    let worst = pixels
        .iter()
        .zip(reference)
        .map(|(a, b)| (*a as i32 - *b as i32).abs())
        .max()
        .unwrap();
    assert!(worst <= 6, "worst difference {worst}");
}

#[test]
fn jpeg_scan_layouts_give_identical_pixels() {
    // One coefficient set, written as one scan, one scan per component, two
    // scans, and with restart markers. The first goes through the allocation-
    // free path and the others through coefficient storage.
    let (reference, _) = decode_image(data!("interleaved_420.jpg"));
    for fixture in [
        data!("restart_420.jpg"),
        data!("multiscan_420.jpg"),
        data!("multiscan_restart_420.jpg"),
        data!("multiscan_y_cbcr_420.jpg"),
        data!("reversed_scan_420.jpg"),
    ] {
        assert_eq!(decode_image(fixture).0, reference);
    }
}

#[test]
fn progressive_jpeg_matches_its_baseline_twin() {
    // libjpeg transcodes to identical coefficients, so the pixels must be equal.
    assert_same_pixels(data!("baseline_420.jpg"), data!("progressive_420.jpg"));
    assert_same_pixels(data!("baseline_444.jpg"), data!("progressive_444.jpg"));
    assert_same_pixels(
        data!("baseline_420.jpg"),
        data!("progressive_restart_420.jpg"),
    );
    assert_same_pixels(data!("baseline_gray.jpg"), data!("progressive_gray.jpg"));
}

#[test]
fn jpeg_colour_signalling() {
    // Adobe transform 0 is RGB. Read as YCbCr it would be far off.
    let (pixels, _) = decode_image(data!("adobe_rgb.jpg"));
    assert!(worst_against_rgb(&pixels, data!("adobe_rgb.rgb")) <= 1);
    // Neither JFIF nor Adobe, with component ids R, G, B.
    let (pixels, _) = decode_image(data!("rgb_ids.jpg"));
    assert!(close(pixel(&pixels, 37, 14, 12), [235, 40, 30], 8));

    // CMYK and YCCK with the Adobe inversion. The references are libjpeg plus
    // Pillow's CMYK conversion, which rounds a little differently.
    let (pixels, _) = decode_image(data!("cmyk.jpg"));
    assert!(worst_against_rgb(&pixels, data!("cmyk.rgb")) <= 2);
    let (pixels, _) = decode_image(data!("ycck.jpg"));
    assert!(worst_against_rgb(&pixels, data!("ycck.rgb")) <= 2);
}

#[test]
fn jpeg_without_dht_uses_the_standard_tables() {
    assert_same_pixels(data!("with_dht.jpg"), data!("no_dht.jpg"));
    let (pixels, info) = decode_image(data!("no_dht_ffmpeg.jpg"));
    assert_eq!((info.width, info.height), (37, 29));
    assert!(close(pixel(&pixels, 37, 14, 12), [235, 40, 30], 30));
}

/// Offset of the SOF marker segment in a JPEG, walking segment lengths.
fn sof_offset(jpeg: &[u8]) -> usize {
    let mut i = 2;
    loop {
        assert_eq!(jpeg[i], 0xff);
        if matches!(jpeg[i + 1], 0xc0 | 0xc1 | 0xc2) {
            return i;
        }
        i += 2 + u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
    }
}

#[test]
fn coefficient_storage_has_a_limit() {
    let mut big = data!("progressive_444.jpg").to_vec();
    let sof = sof_offset(&big);
    // 3000x3000 at 4:4:4 is 54 MB of coefficients for a 36 MB output.
    big[sof + 5..sof + 7].copy_from_slice(&3000u16.to_be_bytes());
    big[sof + 7..sof + 9].copy_from_slice(&3000u16.to_be_bytes());
    let mut out = vec![0u8; 3000 * 3000 * 4];
    assert!(matches!(
        feos_image::jpeg::decode(&big, &mut out),
        Err(ImageError::UnsupportedFeature(_))
    ));
}

#[test]
fn image_end_walks_the_structure() {
    use feos_image::jpeg::image_end;

    for fixture in [
        JPEG,
        JPEG420,
        GRAY_JPEG,
        data!("restart_420.jpg"),
        data!("multiscan_420.jpg"),
        data!("multiscan_restart_420.jpg"),
        data!("progressive_420.jpg"),
        data!("progressive_restart_420.jpg"),
    ] {
        assert_eq!(image_end(fixture), Some(fixture.len()));

        // Bytes after the image, including another image, are not part of it.
        let mut stream = fixture.to_vec();
        stream.extend_from_slice(fixture);
        assert_eq!(image_end(&stream), Some(fixture.len()));

        // A stream cut before EOI has no end, and running into the next image
        // does not borrow its EOI.
        assert_eq!(image_end(&fixture[..fixture.len() - 1]), None);
        let mut cut = fixture[..fixture.len() - 2].to_vec();
        cut.extend_from_slice(fixture);
        assert_eq!(image_end(&cut), None);
    }

    // An EXIF thumbnail is a whole JPEG, EOI included, inside APP1.
    let thumbnail = GRAY_JPEG;
    let mut with_exif = vec![0xff, 0xd8, 0xff, 0xe1];
    with_exif.extend_from_slice(&((thumbnail.len() + 2 + 6) as u16).to_be_bytes());
    with_exif.extend_from_slice(b"Exif\0\0");
    with_exif.extend_from_slice(thumbnail);
    with_exif.extend_from_slice(&JPEG[2..]);
    assert_eq!(image_end(&with_exif), Some(with_exif.len()));
    let (pixels, info) = decode_image(&with_exif);
    assert_eq!((info.width, info.height), (16, 16));
    assert_eq!(pixels, decode_image(JPEG).0);

    assert_eq!(image_end(b""), None);
    assert_eq!(image_end(&[0xff]), None);
    assert_eq!(image_end(b"not a jpeg"), None);
    assert_eq!(image_end(&[0xff, 0xd8]), None);
    // Fill bytes before EOI; a segment running past the input.
    assert_eq!(image_end(&[0xff, 0xd8, 0xff, 0xff, 0xd9, 0]), Some(5));
    assert_eq!(
        image_end(&[0xff, 0xd8, 0xff, 0xe0, 0x40, 0x00, 0xff, 0xd9]),
        None
    );
    // Scan data: stuffing, RSTn and fill bytes are not EOI.
    let scan = [
        0xff, 0xd8, 0xff, 0xda, 0x00, 0x02, 0x12, 0xff, 0x00, 0x34, 0xff, 0xd3, 0x56, 0xff, 0xff,
        0xd9,
    ];
    assert_eq!(image_end(&scan), Some(scan.len()));
}

// ------------------------------------------------------------------------- BMP

#[test]
fn bmp_rle_matches_uncompressed() {
    assert_same_pixels(data!("pal8.bmp"), data!("rle8.bmp"));
    assert_same_pixels(data!("pal4.bmp"), data!("rle4.bmp"));
}

#[test]
fn bmp_rle_rejects_bad_streams() {
    // Run off the right edge of the first row, and a truncated stream.
    let good = data!("rle8.bmp");
    let offset = u32::from_le_bytes([good[10], good[11], good[12], good[13]]) as usize;
    let mut overrun = good.to_vec();
    overrun[offset] = 200;
    let mut out = vec![0u8; 8 * 6 * 4];
    assert!(matches!(
        decode(&overrun, &mut out, &mut []),
        Err(ImageError::InvalidData(_))
    ));
    assert_eq!(
        decode(&good[..offset + 4], &mut out, &mut []),
        Err(ImageError::UnexpectedEnd)
    );
}

fn color_at(x: usize, y: usize) -> [u8; 3] {
    [
        (x * 30 + 10) as u8,
        (y * 40 + 5) as u8,
        (255 - x * 20) as u8,
    ]
}

#[test]
fn bmp_16_bit_masks() {
    // The encoder truncates each channel, and the decoder scales to full range.
    let field = |value: u8, bits: u32| {
        let max = (1u32 << bits) - 1;
        (((value as u32 >> (8 - bits)) * 255 + max / 2) / max) as u8
    };
    let (default_555, _) = decode_image(data!("rgb555.bmp"));
    let (bitfields_565, _) = decode_image(data!("bf565.bmp"));
    for y in 0..6 {
        for x in 0..8 {
            let [r, g, b] = color_at(x, y);
            assert_eq!(
                pixel(&default_555, 8, x, y),
                [field(r, 5), field(g, 5), field(b, 5), 255]
            );
            assert_eq!(
                pixel(&bitfields_565, 8, x, y),
                [field(r, 5), field(g, 6), field(b, 5), 255]
            );
        }
    }
}

#[test]
fn bmp_32_bit_masks_are_honoured() {
    let (rgba, _) = decode_image(data!("bf32_rgba.bmp"));
    let (alpha6, _) = decode_image(data!("bf32_alpha6.bmp"));
    let (opaque, _) = decode_image(data!("bf32_noalpha.bmp"));
    for y in 0..6 {
        for x in 0..8 {
            let [r, g, b] = color_at(x, y);
            let alpha = (x * 36) as u8;
            assert_eq!(pixel(&rgba, 8, x, y), [r, g, b, alpha]);
            assert_eq!(pixel(&alpha6, 8, x, y), [r, g, b, alpha]);
            assert_eq!(pixel(&opaque, 8, x, y), [r, g, b, 255]);
        }
    }
}

#[test]
fn bmp_top_down() {
    let (pixels, _) = decode_image(data!("topdown24.bmp"));
    for y in 0..6 {
        for x in 0..8 {
            let [r, g, b] = color_at(x, y);
            assert_eq!(pixel(&pixels, 8, x, y), [r, g, b, 255]);
        }
    }
}

// ------------------------------------------------------------------------- PNG

#[test]
fn png_rejects_the_wrong_amount_of_image_data() {
    // Both are valid zlib with correct checksums, sized wrong for IHDR.
    for fixture in [data!("short_idat.png"), data!("long_idat.png")] {
        let info = info(fixture).unwrap();
        let mut out = vec![0u8; info.rgba_len().unwrap()];
        let mut scratch = vec![0u8; scratch_len(fixture).unwrap()];
        assert!(matches!(
            decode(fixture, &mut out, &mut scratch),
            Err(ImageError::InvalidData(_))
        ));
    }
}

#[test]
fn png_16_bit_keeps_the_high_byte() {
    let (rgb, _) = decode_image(data!("rgb16.png"));
    let (rgba, _) = decode_image(data!("rgba16.png"));
    for y in 0..5 {
        for x in 0..6 {
            let [r, g, b] = color_at(x, y);
            assert_eq!(pixel(&rgb, 6, x, y), [r, g, b, 255]);
            assert_eq!(pixel(&rgba, 6, x, y), [r, g, b, (x * 40) as u8]);
        }
    }
    let (adam7, _) = decode_image(data!("adam7_rgb16.png"));
    assert_eq!(adam7, rgb);
    let (split, _) = decode_image(data!("idat_split.png"));
    assert_eq!(split, rgb);
}

#[test]
fn png_trns_on_gray_and_rgb() {
    let (gray, _) = decode_image(data!("trns_gray8.png"));
    assert_eq!(gray, [10, 10, 10, 255, 11, 11, 11, 0, 12, 12, 12, 255]);
    // Same high bytes: only a full 16-bit compare tells them apart.
    let (rgb, _) = decode_image(data!("trns_rgb16.png"));
    assert_eq!(rgb, [0x12, 0x56, 0x9a, 255, 0x12, 0x56, 0x9a, 0]);
}

#[test]
fn png_packed_gray() {
    for (fixture, depth) in [
        (data!("gray1.png"), 1u32),
        (data!("gray2.png"), 2),
        (data!("gray4.png"), 4),
    ] {
        let (pixels, _) = decode_image(fixture);
        let max = (1usize << depth) - 1;
        for y in 0..4 {
            for x in 0..8 {
                let value = ((x + y) & max) * 255 / max;
                assert_eq!(
                    pixel(&pixels, 8, x, y),
                    [value as u8, value as u8, value as u8, 255]
                );
            }
        }
    }
}

// ---------------------------------------------------------------- Netpbm, QOI

#[test]
fn netpbm_ascii_matches_binary() {
    assert_same_pixels(data!("p1.pbm"), data!("p4.pbm"));
    assert_same_pixels(data!("p2.pgm"), data!("p5.pgm"));
    assert_same_pixels(data!("p3.ppm"), data!("p6.ppm"));
    for fixture in [
        data!("p1.pbm"),
        data!("p2.pgm"),
        data!("p3.ppm"),
        data!("p4.pbm"),
    ] {
        assert_eq!(detect(fixture), Some(ImageFormat::Ppm));
    }

    // A PBM's 1 is black: (x + y) odd.
    let (bits, _) = decode_image(data!("p4.pbm"));
    assert_eq!(pixel(&bits, 5, 0, 0), [255, 255, 255, 255]);
    assert_eq!(pixel(&bits, 5, 1, 0), [0, 0, 0, 255]);

    let (gray, _) = decode_image(data!("p5.pgm"));
    let (gray16, _) = decode_image(data!("p5_16.pgm"));
    for (a, b) in gray.iter().zip(&gray16) {
        assert!((*a as i32 - *b as i32).abs() <= 1);
    }
}

#[test]
fn netpbm_rejects_bad_input() {
    // Text starting with the letters of a magic number is not an image.
    assert_eq!(detect(b"P1"), None);
    assert_eq!(detect(b"P100 is a part number"), None);
    let mut out = [0u8; 64];
    // Too few samples, a non-digit bit, and a sample over maxval saturates.
    assert_eq!(
        decode(b"P2 2 1 255 7", &mut out, &mut []),
        Err(ImageError::UnexpectedEnd)
    );
    assert!(matches!(
        decode(b"P1 2 1 0x", &mut out, &mut []),
        Err(ImageError::InvalidData(_))
    ));
    decode(b"P2 1 1 15 4000000000", &mut out, &mut []).unwrap();
    assert_eq!(out[..4], [255, 255, 255, 255]);
}

#[test]
fn decodes_ffmpeg_qoi() {
    let (rgb, info) = decode_image(data!("rgb.qoi"));
    assert_eq!(info.format, ImageFormat::Qoi);
    assert_eq!(rgb, decode_image(PPM).0);
    let (rgba, _) = decode_image(data!("rgba.qoi"));
    assert_eq!(rgba, decode_image(RGBA_PNG).0);
}
