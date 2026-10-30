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
