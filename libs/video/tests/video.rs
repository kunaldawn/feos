use feos_video::{decode_frame, detect, info, scratch_len, Codec, ContainerFormat, VideoError};

const GIF: &[u8] = include_bytes!("data/anim.gif");
const GIF_0: &[u8] = include_bytes!("data/ref_gif_0.rgba");
const GIF_1: &[u8] = include_bytes!("data/ref_gif_1.rgba");
const AVI_RAW: &[u8] = include_bytes!("data/raw.avi");
const AVI_RAW_0: &[u8] = include_bytes!("data/ref_raw_0.rgba");
const AVI_MJPEG: &[u8] = include_bytes!("data/mjpeg.avi");
const AVI_MJPEG_0: &[u8] = include_bytes!("data/ref_mjpeg_0.rgba");
const MP4: &[u8] = include_bytes!("data/h264.mp4");
const MP4_0: &[u8] = include_bytes!("data/ref_mp4_0.rgba");
const MP4_1: &[u8] = include_bytes!("data/ref_mp4_1.rgba");
const MKV: &[u8] = include_bytes!("data/h264.mkv");
const MKV_0: &[u8] = include_bytes!("data/ref_mkv_0.rgba");
const FLV: &[u8] = include_bytes!("data/h264.flv");
const FLV_0: &[u8] = include_bytes!("data/ref_flv_0.rgba");
const ANNEX_B: &[u8] = include_bytes!("data/h264.h264");
const ANNEX_B_0: &[u8] = include_bytes!("data/ref_annexb_0.rgba");
const VP8: &[u8] = include_bytes!("data/vp8.webm");
const VP9: &[u8] = include_bytes!("data/vp9.webm");
const IVF: &[u8] = include_bytes!("data/vp9.ivf");

fn decode_index(input: &[u8], index: u32) -> (Vec<u8>, feos_video::DecodedFrame) {
    let props = info(input).unwrap();
    let mut out = vec![0u8; props.rgba_len().unwrap()];
    let mut scratch = vec![0u8; scratch_len(input).unwrap()];
    let frame = decode_frame(input, index, &mut out, &mut scratch).unwrap();
    (out, frame)
}

fn max_channel_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| x.abs_diff(y))
        .max()
        .unwrap_or(0)
}

#[test]
fn detects_every_container() {
    assert_eq!(detect(GIF), Some(ContainerFormat::Gif));
    assert_eq!(detect(AVI_RAW), Some(ContainerFormat::Avi));
    assert_eq!(detect(AVI_MJPEG), Some(ContainerFormat::Avi));
    assert_eq!(detect(MP4), Some(ContainerFormat::Mp4));
    assert_eq!(detect(MKV), Some(ContainerFormat::Matroska));
    assert_eq!(detect(FLV), Some(ContainerFormat::Flv));
    assert_eq!(detect(IVF), Some(ContainerFormat::Ivf));
    assert_eq!(detect(ANNEX_B), Some(ContainerFormat::H264));
    assert_eq!(detect(b"not video"), None);
    assert_eq!(info(b"not video"), Err(VideoError::UnknownFormat));
}

#[test]
fn reads_animated_gif() {
    let props = info(GIF).unwrap();
    assert_eq!(props.format, ContainerFormat::Gif);
    assert_eq!(props.codec, Codec::Gif);
    assert_eq!((props.width, props.height), (32, 32));
    assert_eq!(props.frame_count, 4);
    assert_eq!(scratch_len(GIF).unwrap(), 32 * 32 * 5);

    let (first, frame) = decode_index(GIF, 0);
    assert_eq!(frame.width, 32);
    assert_eq!(frame.height, 32);
    assert_eq!(max_channel_diff(&first, GIF_0), 0);

    let (second, _) = decode_index(GIF, 1);
    assert_eq!(max_channel_diff(&second, GIF_1), 0);
}

#[test]
fn reads_uncompressed_avi() {
    let props = info(AVI_RAW).unwrap();
    assert_eq!(props.codec, Codec::RawBgr);
    assert_eq!((props.width, props.height), (32, 32));
    assert_eq!(props.frame_count, 4);
    assert_eq!(props.frame_rate, Some((10, 1)));

    let (pixels, _) = decode_index(AVI_RAW, 0);
    assert_eq!(max_channel_diff(&pixels, AVI_RAW_0), 0);
}

#[test]
fn reads_mjpeg_avi() {
    let props = info(AVI_MJPEG).unwrap();
    assert_eq!(props.codec, Codec::Mjpeg);
    assert_eq!((props.width, props.height), (32, 32));
    assert_eq!(props.frame_count, 4);

    let (pixels, _) = decode_index(AVI_MJPEG, 0);
    // The JPEG decoder and FFmpeg use different IDCT rounding.
    assert!(max_channel_diff(&pixels, AVI_MJPEG_0) <= 12);
}

#[test]
fn decodes_h264_in_mp4() {
    let props = info(MP4).unwrap();
    assert_eq!(props.format, ContainerFormat::Mp4);
    assert_eq!(props.codec, Codec::H264);
    assert_eq!((props.width, props.height), (32, 32));
    assert_eq!(props.frame_count, 4);
    assert_eq!(props.frame_rate, Some((10, 1)));

    let (first, _) = decode_index(MP4, 0);
    assert!(max_channel_diff(&first, MP4_0) <= 12);
    let (second, _) = decode_index(MP4, 1);
    assert!(max_channel_diff(&second, MP4_1) <= 12);
    assert_ne!(first, second);
}

#[test]
fn decodes_h264_in_matroska() {
    let props = info(MKV).unwrap();
    assert_eq!(props.format, ContainerFormat::Matroska);
    assert_eq!(props.codec, Codec::H264);
    assert_eq!((props.width, props.height), (32, 32));
    assert_eq!(props.frame_count, 4);
    assert_eq!(props.frame_rate, Some((10, 1)));

    let (pixels, _) = decode_index(MKV, 0);
    assert!(max_channel_diff(&pixels, MKV_0) <= 12);
}

#[test]
fn decodes_h264_in_flv() {
    let props = info(FLV).unwrap();
    assert_eq!(props.format, ContainerFormat::Flv);
    assert_eq!(props.codec, Codec::H264);
    assert_eq!((props.width, props.height), (32, 32));
    assert!(props.frame_count >= 4);
    assert_eq!(props.frame_rate, Some((10, 1)));

    let (pixels, _) = decode_index(FLV, 0);
    assert!(max_channel_diff(&pixels, FLV_0) <= 12);
}

#[test]
fn decodes_annex_b_h264() {
    let props = info(ANNEX_B).unwrap();
    assert_eq!(props.format, ContainerFormat::H264);
    assert_eq!(props.codec, Codec::H264);
    assert_eq!((props.width, props.height), (32, 32));

    let (pixels, _) = decode_index(ANNEX_B, 0);
    assert!(max_channel_diff(&pixels, ANNEX_B_0) <= 12);
}

#[test]
fn reports_modern_codecs_without_pixels() {
    let vp8 = info(VP8).unwrap();
    assert_eq!(vp8.codec, Codec::Vp8);
    assert_eq!((vp8.width, vp8.height), (32, 32));

    let vp9 = info(VP9).unwrap();
    assert_eq!(vp9.codec, Codec::Vp9);
    assert_eq!((vp9.width, vp9.height), (32, 32));

    let ivf = info(IVF).unwrap();
    assert_eq!(ivf.format, ContainerFormat::Ivf);
    assert_eq!(ivf.codec, Codec::Vp9);
    assert_eq!(ivf.frame_count, 4);
    assert_eq!(ivf.frame_rate, Some((10, 1)));

    let mut out = vec![0u8; ivf.rgba_len().unwrap()];
    let mut scratch = vec![0u8; scratch_len(IVF).unwrap()];
    assert_eq!(
        decode_frame(IVF, 0, &mut out, &mut scratch),
        Err(VideoError::UnsupportedCodec("vp9"))
    );
}

#[test]
fn rejects_out_of_range_frames() {
    let props = info(MP4).unwrap();
    let mut out = vec![0u8; props.rgba_len().unwrap()];
    let mut scratch = vec![0u8; scratch_len(MP4).unwrap()];
    assert_eq!(
        decode_frame(MP4, 99, &mut out, &mut scratch),
        Err(VideoError::FrameOutOfRange {
            index: 99,
            count: 4,
        })
    );
}
