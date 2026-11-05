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
    // The trailer is CRC-32 then ISIZE, so the last four bytes don't exercise
    // the CRC comparison at all.
    let mut damaged = GZIP.to_vec();
    let crc = damaged.len() - 8;
    damaged[crc] ^= 0xff;
    let mut out = vec![0u8; PLAIN.len()];
    assert_eq!(
        gzip::decompress(&damaged, &mut out),
        Err(CompressError::Checksum)
    );
}

#[test]
fn rejects_bad_gzip_size_field() {
    let mut damaged = GZIP.to_vec();
    let last = damaged.len() - 1;
    damaged[last] ^= 0xff;
    let mut out = vec![0u8; PLAIN.len()];
    assert_eq!(
        gzip::decompress(&damaged, &mut out),
        Err(CompressError::Checksum)
    );
}

/// A gzip member around one stored deflate block, so flipping a payload byte
/// can only be caught by the CRC.
fn stored_gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    out.push(1);
    out.extend_from_slice(&(data.len() as u16).to_le_bytes());
    out.extend_from_slice(&(!(data.len() as u16)).to_le_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&checksum::crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

#[test]
fn crc_catches_corrupted_payload() {
    let good = stored_gzip(b"payload bytes");
    let mut out = vec![0u8; 64];
    assert_eq!(gzip::decompress(&good, &mut out), Ok(13));
    let mut damaged = good;
    damaged[17] ^= 0x01;
    assert_eq!(
        gzip::decompress(&damaged, &mut out),
        Err(CompressError::Checksum)
    );
}

#[test]
fn adler_catches_corrupted_payload() {
    let data = b"payload bytes";
    let mut stream = vec![0x78, 0x01, 1];
    stream.extend_from_slice(&(data.len() as u16).to_le_bytes());
    stream.extend_from_slice(&(!(data.len() as u16)).to_le_bytes());
    stream.extend_from_slice(data);
    stream.extend_from_slice(&checksum::adler32(data).to_be_bytes());
    let mut out = vec![0u8; 64];
    assert_eq!(zlib::decompress(&stream, &mut out), Ok(data.len()));
    stream[8] ^= 0x01;
    assert_eq!(
        zlib::decompress(&stream, &mut out),
        Err(CompressError::Checksum)
    );
}

/// The classic byte-at-a-time CRC-32, as the reference for slicing-by-8.
fn reference_crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xedb8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[test]
fn crc32_matches_the_bytewise_definition_at_every_length_and_split() {
    let mut state = 0x1234_5678u32;
    let data: Vec<u8> = (0..4099)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect();
    for len in (0..64).chain([255, 256, 257, 1024, 4099]) {
        assert_eq!(checksum::crc32(&data[..len]), reference_crc32(&data[..len]));
        for split in [0, 1, 3, 7, 8, 9, len / 2, len] {
            let split = split.min(len);
            let mut crc = checksum::Crc32::new();
            crc.update(&data[..split]);
            crc.update(&data[split..len]);
            assert_eq!(crc.finish(), reference_crc32(&data[..len]), "{len}/{split}");
        }
    }
}

/// Writes bits LSB first, as DEFLATE packs them.
struct BitWriter {
    out: Vec<u8>,
    bits: u64,
    count: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            bits: 0,
            count: 0,
        }
    }

    fn put(&mut self, value: u32, n: u32) {
        self.bits |= (value as u64) << self.count;
        self.count += n;
        while self.count >= 8 {
            self.out.push(self.bits as u8);
            self.bits >>= 8;
            self.count -= 8;
        }
    }

    /// Writes a Huffman code, which DEFLATE packs most significant bit first.
    fn code(&mut self, code: u32, n: u32) {
        for bit in (0..n).rev() {
            self.put((code >> bit) & 1, 1);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.out.push(self.bits as u8);
        }
        self.out
    }
}

/// One dynamic block whose code lengths run from 1 to 15 bits, so the decoder
/// must agree with itself across the fast table and the long-code walk.
#[test]
fn inflates_codes_longer_than_the_lookup_table() {
    // A complete code: 'a' is 1 bit, 'b' 2 bits, ... 'o' 15 bits, and
    // end-of-block shares 15 bits.
    let alphabet: Vec<u8> = (0..15).map(|index| b'a' + index).collect();
    let mut lengths = [0u8; 257];
    for (index, &symbol) in alphabet.iter().enumerate() {
        lengths[symbol as usize] = index as u8 + 1;
    }
    lengths[256] = 15;

    let mut writer = BitWriter::new();
    writer.put(1, 1); // final block
    writer.put(2, 2); // dynamic
    writer.put(0, 5); // HLIT = 257
    writer.put(0, 5); // HDIST = 1
    writer.put(15, 4); // HCLEN = 19

    // Code-length symbols 0..=15 all take four bits, so each symbol's code is
    // simply its own value.
    for symbol in [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ] {
        writer.put(if symbol <= 15 { 4 } else { 0 }, 3);
    }
    for &length in &lengths {
        writer.code(length as u32, 4);
    }
    writer.code(1, 4); // the single, unused distance code

    let mut literal_codes = [(0u32, 0u32); 257];
    let mut code = 0u32;
    for bits in 1..=15u8 {
        for symbol in 0..lengths.len() {
            if lengths[symbol] == bits {
                literal_codes[symbol] = (code, bits as u32);
                code += 1;
            }
        }
        code <<= 1;
    }

    let mut plain = Vec::new();
    for repeat in 0..40 {
        plain.extend_from_slice(&alphabet);
        plain.push(alphabet[repeat % alphabet.len()]);
    }
    for &symbol in &plain {
        let (code, bits) = literal_codes[symbol as usize];
        writer.code(code, bits);
    }
    let (code, bits) = literal_codes[256];
    writer.code(code, bits);
    let stream = writer.finish();

    let mut out = vec![0u8; plain.len()];
    let result = inflate::inflate_with_consumed(&stream, &mut out).unwrap();
    assert_eq!(&out[..result.written], &plain[..]);
    assert_eq!(result.consumed, stream.len());

    // Every truncation must report it, never read past the input or panic.
    for cut in 0..stream.len() {
        assert_eq!(
            inflate::inflate(&stream[..cut], &mut out),
            Err(CompressError::UnexpectedEnd),
            "cut at {cut}"
        );
    }
}

#[test]
fn consumed_stops_at_the_end_of_the_stream() {
    let mut stream = RAW.to_vec();
    let raw_len = stream.len();
    stream.extend_from_slice(&[0xaa; 32]);
    let mut out = vec![0u8; PLAIN.len()];
    let result = inflate::inflate_with_consumed(&stream, &mut out).unwrap();
    assert_eq!(result.consumed, raw_len);
    assert_eq!(&out[..result.written], PLAIN);
}

#[test]
fn inflate_handles_overlapping_and_distant_matches() {
    let mut plain = Vec::new();
    for index in 0..30_000u32 {
        plain.push((index % 7) as u8 + b'a');
    }
    plain.extend_from_slice(&[b'z'; 5000]);
    let mut compressed = vec![0u8; plain.len() + 1024];
    let mut deflater = Deflater::new();
    let len = deflate::compress_with(&mut deflater, &plain, &mut compressed).unwrap();
    let mut out = vec![0u8; plain.len()];
    assert_eq!(
        inflate::inflate(&compressed[..len], &mut out),
        Ok(plain.len())
    );
    assert_eq!(out, plain);
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

#[test]
fn gzip_streaming_agrees_with_one_shot_and_checks_the_trailer() {
    use feos_compress::{gzip, CompressError, Result, WINDOW_SIZE};
    let data: Vec<u8> = (0..120_000u32).map(|value| (value / 7) as u8).collect();
    let mut packed = vec![0u8; data.len() + 4096];
    let len = gzip::compress(&data, &mut packed).unwrap();
    packed.truncate(len);
    // Two members are one stream.
    let mut twice = packed.clone();
    twice.extend_from_slice(&packed);

    let run = |input: &[u8]| -> Result<Vec<u8>> {
        let mut at = 0;
        let mut read = |buffer: &mut [u8]| -> Result<usize> {
            let take = buffer.len().min(1000).min(input.len() - at);
            buffer[..take].copy_from_slice(&input[at..at + take]);
            at += take;
            Ok(take)
        };
        let mut out = Vec::new();
        let mut write = |chunk: &[u8]| -> Result<()> {
            out.extend_from_slice(chunk);
            Ok(())
        };
        let mut window = vec![0u8; WINDOW_SIZE];
        gzip::decompress_stream(&mut read, &mut window, &mut write)?;
        Ok(out)
    };
    assert_eq!(run(&packed).unwrap(), data);
    let mut expected = data.clone();
    expected.extend_from_slice(&data);
    assert_eq!(run(&twice).unwrap(), expected);

    let mut damaged = packed.clone();
    let last = damaged.len() - 6;
    damaged[last] ^= 1;
    assert_eq!(run(&damaged), Err(CompressError::Checksum));
    assert_eq!(run(&packed[..packed.len() - 3]), Err(CompressError::UnexpectedEnd));
    assert_eq!(run(&[]), Err(CompressError::UnexpectedEnd));
    assert!(run(b"not gzip at all, sorry").is_err());
}
