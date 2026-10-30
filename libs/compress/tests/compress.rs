use feos_compress::{checksum, deflate, gzip, inflate, zlib, CompressError, Deflater};

const PLAIN: &[u8] = include_bytes!("data/plain.bin");
const RAW: &[u8] = include_bytes!("data/raw.deflate");
const ZLIB: &[u8] = include_bytes!("data/zlib.bin");
const GZIP: &[u8] = include_bytes!("data/gzip.bin");
const GZIP_NAMED: &[u8] = include_bytes!("data/gzip_named.bin");

#[test]
fn checksums() {
    assert_eq!(checksum::crc32(b"123456789"), 0xcbf4_3926);
    assert_eq!(checksum::adler32(b"Wikipedia"), 0x11e6_0398);

    let mut crc = checksum::Crc32::new();
    crc.update(b"1234");
    crc.update(b"56789");
    assert_eq!(crc.finish(), 0xcbf4_3926);
}

#[test]
fn inflates_python_deflate() {
    let mut out = vec![0u8; PLAIN.len()];
    let result = inflate::inflate_with_consumed(RAW, &mut out).unwrap();
    assert_eq!(result.written, PLAIN.len());
    assert_eq!(result.consumed, RAW.len());
    assert_eq!(out, PLAIN);
}

#[test]
fn inflates_zlib() {
    let mut out = vec![0u8; PLAIN.len()];
    let written = zlib::decompress(ZLIB, &mut out).unwrap();
    assert_eq!(written, PLAIN.len());
    assert_eq!(out, PLAIN);
}

#[test]
fn inflates_gzip() {
    let mut out = vec![0u8; PLAIN.len()];
    let written = gzip::decompress(GZIP, &mut out).unwrap();
    assert_eq!(written, PLAIN.len());
    assert_eq!(out, PLAIN);
}

#[test]
fn inflates_named_gzip() {
    let mut out = vec![0u8; PLAIN.len()];
    let written = gzip::decompress(GZIP_NAMED, &mut out).unwrap();
    assert_eq!(written, PLAIN.len());
    assert_eq!(out, PLAIN);
}

fn roundtrip(data: &[u8]) {
    let mut compressed = vec![0u8; deflate::bound(data.len()) + 16];
    let len = deflate::compress(data, &mut compressed).unwrap();
    let mut out = vec![0u8; data.len()];
    let written = inflate::inflate(&compressed[..len], &mut out).unwrap();
    assert_eq!(written, data.len());
    assert_eq!(out, data);

    let len = zlib::compress(data, &mut compressed).unwrap();
    let written = zlib::decompress(&compressed[..len], &mut out).unwrap();
    assert_eq!(written, data.len());
    assert_eq!(out, data);

    let len = gzip::compress(data, &mut compressed).unwrap();
    let written = gzip::decompress(&compressed[..len], &mut out).unwrap();
    assert_eq!(written, data.len());
    assert_eq!(out, data);
}

#[test]
fn roundtrips() {
    roundtrip(b"");
    roundtrip(b"a");
    roundtrip(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    roundtrip(b"The quick brown fox jumps over the lazy dog.");
    roundtrip(PLAIN);

    let pattern: Vec<u8> = (0..100_000u32)
        .map(|i| ((i * 37) ^ (i >> 3)) as u8)
        .collect();
    roundtrip(&pattern);

    let zeros = vec![0u8; 80_000];
    roundtrip(&zeros);
}

#[test]
fn reuses_deflater() {
    let mut deflater = Deflater::new();
    let mut compressed = vec![0u8; 4096];
    let mut out = vec![0u8; PLAIN.len()];
    for _ in 0..8 {
        let len = deflate::compress_with(&mut deflater, PLAIN, &mut compressed).unwrap();
        let written = inflate::inflate(&compressed[..len], &mut out).unwrap();
        assert_eq!(&out[..written], PLAIN);
    }
}

#[test]
fn rejects_bad_checksum() {
    let mut damaged = GZIP.to_vec();
    let last = damaged.len() - 1;
    damaged[last] ^= 0xff;
    let mut out = vec![0u8; PLAIN.len()];
    assert_eq!(
        gzip::decompress(&damaged, &mut out),
        Err(CompressError::Checksum)
    );
}

#[test]
fn rejects_short_output() {
    let mut out = vec![0u8; 4];
    assert!(matches!(
        inflate::inflate(RAW, &mut out),
        Err(CompressError::BufferTooSmall { .. })
    ));
}

#[test]
fn rejects_truncated_input() {
    let mut out = vec![0u8; PLAIN.len()];
    assert_eq!(
        inflate::inflate(&RAW[..RAW.len() - 2], &mut out),
        Err(CompressError::UnexpectedEnd)
    );
}

#[test]
fn inflates_gzip_with_all_optional_header_fields() {
    let mut member = vec![0x1f, 0x8b, 0x08, 0x1e, 0, 0, 0, 0, 0, 0xff];
    member.extend_from_slice(&4u16.to_le_bytes());
    member.extend_from_slice(b"ABCD");
    member.extend_from_slice(b"name.bin\0");
    member.extend_from_slice(b"a comment\0");
    let header_crc = (checksum::crc32(&member) & 0xffff) as u16;
    member.extend_from_slice(&header_crc.to_le_bytes());
    member.extend_from_slice(RAW);
    member.extend_from_slice(&checksum::crc32(PLAIN).to_le_bytes());
    member.extend_from_slice(&(PLAIN.len() as u32).to_le_bytes());

    let mut out = vec![0u8; PLAIN.len()];
    let written = gzip::decompress(&member, &mut out).unwrap();
    assert_eq!(written, PLAIN.len());
    assert_eq!(out, PLAIN);
}

#[test]
fn concatenated_gzip_members() {
    let mut two = GZIP.to_vec();
    two.extend_from_slice(GZIP);
    let mut out = vec![0u8; PLAIN.len() * 2];
    let written = gzip::decompress(&two, &mut out).unwrap();
    assert_eq!(written, PLAIN.len() * 2);
    assert_eq!(&out[..PLAIN.len()], PLAIN);
    assert_eq!(&out[PLAIN.len()..], PLAIN);
}
