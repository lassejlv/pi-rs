//! Header-only JPEG/WebP EXIF orientation, including padded RIFF chunks.
fn ascii(bytes: &[u8], at: usize, text: &[u8]) -> bool {
    bytes.get(at..at + text.len()) == Some(text)
}
fn exif(bytes: &[u8], at: usize) -> bool {
    ascii(bytes, at, b"Exif\0\0")
}
fn u16_at(bytes: &[u8], at: usize, little: bool) -> u32 {
    let a = u32::from(*bytes.get(at).unwrap_or(&0));
    let b = u32::from(*bytes.get(at + 1).unwrap_or(&0));
    if little { a | (b << 8) } else { (a << 8) | b }
}
fn u32_at(bytes: &[u8], at: usize, little: bool) -> u32 {
    (0..4).fold(0, |out, i| {
        out | (u32::from(*bytes.get(at + i).unwrap_or(&0)) << (8 * if little { i } else { 3 - i }))
    })
}
fn tiff_start(bytes: &[u8]) -> Option<usize> {
    if bytes.starts_with(&[255, 216]) {
        let mut at = 2;
        while at + 4 <= bytes.len() {
            if bytes[at] != 255 {
                return None;
            }
            let marker = bytes[at + 1];
            if marker == 255 {
                at += 1;
                continue;
            }
            if marker == 225 && exif(bytes, at + 4) {
                return Some(at + 10);
            }
            at += 2 + u16_at(bytes, at + 2, false) as usize;
        }
        return None;
    }
    if ascii(bytes, 0, b"RIFF") && ascii(bytes, 8, b"WEBP") {
        let mut at = 12;
        while at + 8 <= bytes.len() {
            let size = u64::from(u32_at(bytes, at + 4, true));
            if ascii(bytes, at, b"EXIF") {
                return Some(if exif(bytes, at + 8) { at + 14 } else { at + 8 });
            }
            let next = at as u64 + 8 + size + size % 2;
            if next > bytes.len() as u64 {
                break;
            }
            at = next as usize;
        }
    }
    None
}
pub fn exif_orientation(bytes: &[u8]) -> u8 {
    let Some(tiff) = tiff_start(bytes) else {
        return 1;
    };
    if tiff + 8 > bytes.len() {
        return 1;
    }
    let little = ascii(bytes, tiff, b"II");
    let directory = tiff as u64 + u64::from(u32_at(bytes, tiff + 4, little));
    if directory + 2 > bytes.len() as u64 {
        return 1;
    }
    let directory = directory as usize;
    let entries = u16_at(bytes, directory, little);
    for i in 0..entries {
        let at = directory + 2 + i as usize * 12;
        if at + 12 > bytes.len() {
            return 1;
        }
        if u16_at(bytes, at, little) == 0x112 {
            let value = u16_at(bytes, at + 8, little);
            return if (1..=8).contains(&value) {
                value as u8
            } else {
                1
            };
        }
    }
    1
}
