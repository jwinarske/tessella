//! A minimal PNG writer.
//!
//! Written rather than depended upon because the workspace already carries `flate2` and a PNG
//! *decoder*, and what is needed here is forty lines of framing around a zlib stream. A whole
//! encoder crate for one debug output would show up in §12.3's binary budget for nothing.

use std::io::Write as _;

use flate2::Compression;
use flate2::write::ZlibEncoder;

/// Encodes 8-bit RGBA rows as a PNG.
pub(crate) fn encode(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    // Each scanline is prefixed with its filter byte. Zero — "None" — because the data is already
    // going through zlib and a filter that has to be chosen per row is a compression decision, not
    // a correctness one.
    let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
    for row in 0..height as usize {
        raw.push(0);
        let start = row * width as usize * 4;
        raw.extend_from_slice(&rgba[start..start + width as usize * 4]);
    }

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&raw).expect("a vector never fails");
    let compressed = encoder.finish().expect("a vector never fails");

    let mut out = Vec::with_capacity(compressed.len() + 64);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    // Eight bits per channel, colour type 6 (RGBA), deflate, no filter, no interlace.
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &compressed);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

/// The CRC-32 the PNG specification names, built on first use rather than tabulated by hand.
struct Crc {
    value: u32,
}

impl Crc {
    fn new() -> Self {
        Self { value: 0xffff_ffff }
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            let mut value = (self.value ^ u32::from(*byte)) & 0xff;
            for _ in 0..8 {
                value = if value & 1 == 1 {
                    0xedb8_8320 ^ (value >> 1)
                } else {
                    value >> 1
                };
            }
            self.value = value ^ (self.value >> 8);
        }
    }

    fn finish(self) -> u32 {
        self.value ^ 0xffff_ffff
    }
}
