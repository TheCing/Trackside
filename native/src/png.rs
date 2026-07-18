//! Minimal RGBA8 → PNG encoder — zero dependencies.
//!
//! We already hand-roll BC decode, FNV hashing, etc., so rather than pull an image crate
//! (and a DEFLATE compressor) into the DLL just to eyeball texture dumps, we emit PNG using
//! **stored** (uncompressed) DEFLATE blocks. The result is a valid PNG that opens anywhere;
//! it's larger than a compressed encoder's output, but these are throwaway diagnostic dumps
//! so size doesn't matter. Input is tightly-packed top-down RGBA8 (width*height*4 bytes).

use std::sync::OnceLock;

fn crc_table() -> &'static [u32; 256] {
    static T: OnceLock<[u32; 256]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 256];
        let mut n = 0usize;
        while n < 256 {
            let mut c = n as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
                k += 1;
            }
            t[n] = c;
            n += 1;
        }
        t
    })
}

fn crc32(bytes: &[u8]) -> u32 {
    let t = crc_table();
    let mut c = 0xffff_ffffu32;
    for &b in bytes {
        c = t[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffff_ffff
}

fn adler32(bytes: &[u8]) -> u32 {
    // Largest n before 16-bit overflow of `b` is 5552 (per the zlib spec).
    let mut a = 1u32;
    let mut b = 0u32;
    for chunk in bytes.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_in = Vec::with_capacity(4 + data.len());
    crc_in.extend_from_slice(kind);
    crc_in.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
}

/// Encode tightly-packed top-down RGBA8 pixels as a PNG byte stream.
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let stride = width as usize * 4;
    // Raw image data = each scanline prefixed with a filter byte (0 = none).
    let mut raw = Vec::with_capacity(height as usize * (1 + stride));
    for y in 0..height as usize {
        raw.push(0);
        let off = y * stride;
        raw.extend_from_slice(&rgba[off..off + stride]);
    }

    // zlib stream wrapping stored DEFLATE blocks (max 65535 bytes each).
    let mut zlib = Vec::with_capacity(raw.len() + raw.len() / 65535 * 5 + 16);
    zlib.extend_from_slice(&[0x78, 0x01]); // CM=deflate, no dictionary
    let mut i = 0;
    if raw.is_empty() {
        zlib.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]); // one empty final block
    }
    while i < raw.len() {
        let end = (i + 65535).min(raw.len());
        let chunk = &raw[i..end];
        let len = chunk.len() as u16;
        zlib.push(if end == raw.len() { 0x01 } else { 0x00 }); // BFINAL on last
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(chunk);
        i = end;
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = Vec::with_capacity(zlib.len() + 64);
    png.extend_from_slice(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]); // signature

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, no-filter, no-interlace
    write_chunk(&mut png, b"IHDR", &ihdr);
    write_chunk(&mut png, b"IDAT", &zlib);
    write_chunk(&mut png, b"IEND", &[]);
    png
}
