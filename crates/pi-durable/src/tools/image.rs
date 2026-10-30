use super::image_processor::ImageDimensions;
use crate::{Context, Result, env::BinaryReader};
const PNG: &[u8] = &[137, 80, 78, 71, 13, 10, 26, 10];
fn ascii(b: &[u8], offset: usize, text: &[u8]) -> bool {
    b.get(offset..offset + text.len()) == Some(text)
}
fn u16le(b: &[u8], at: usize) -> u32 {
    u32::from(*b.get(at).unwrap_or(&0)) + (u32::from(*b.get(at + 1).unwrap_or(&0)) << 8)
}
fn u16be(b: &[u8], at: usize) -> u32 {
    (u32::from(*b.get(at).unwrap_or(&0)) << 8) + u32::from(*b.get(at + 1).unwrap_or(&0))
}
fn u32be(b: &[u8], at: usize) -> u32 {
    (0..4).fold(0, |out, i| {
        (out << 8) | u32::from(*b.get(at + i).unwrap_or(&0))
    })
}
fn u32le(b: &[u8], at: usize) -> u32 {
    (0..4).fold(0, |out, i| {
        out | (u32::from(*b.get(at + i).unwrap_or(&0)) << (8 * i))
    })
}
fn png(b: &[u8]) -> bool {
    b.len() >= 16 && u32be(b, 8) == 13 && ascii(b, 12, b"IHDR")
}
fn bmp(b: &[u8]) -> bool {
    if b.len() < 26 {
        return false;
    }
    let size = u32le(b, 2);
    let offset = u32le(b, 10);
    let dib = u32le(b, 14);
    if size != 0 && size < 26
        || u64::from(offset) < 14 + u64::from(dib)
        || size != 0 && offset >= size
    {
        return false;
    }
    let (planes, bits) = if dib == 12 {
        (u16le(b, 22), u16le(b, 24))
    } else if (40..=124).contains(&dib) && b.len() >= 30 {
        (u16le(b, 26), u16le(b, 28))
    } else {
        return false;
    };
    planes == 1 && matches!(bits, 1 | 4 | 8 | 16 | 24 | 32)
}
fn animated(b: &[u8]) -> bool {
    let mut at = 8;
    while at + 8 <= b.len() {
        if ascii(b, at + 4, b"acTL") {
            return true;
        }
        if ascii(b, at + 4, b"IDAT") {
            return false;
        }
        let next = at as u64 + 12 + u64::from(u32be(b, at));
        if next > b.len() as u64 {
            return false;
        }
        at = next as usize;
    }
    false
}
pub fn detect_supported_image_mime_type(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[255, 216, 255]) {
        return (b.get(3) != Some(&247)).then_some("image/jpeg");
    }
    if b.starts_with(PNG) {
        return (png(b) && !animated(b)).then_some("image/png");
    }
    if ascii(b, 0, b"GIF87a") || ascii(b, 0, b"GIF89a") {
        return Some("image/gif");
    }
    if ascii(b, 0, b"RIFF") && ascii(b, 8, b"WEBP") {
        return Some("image/webp");
    }
    (ascii(b, 0, b"BM") && bmp(b)).then_some("image/bmp")
}
pub async fn detect_supported_image_mime_type_of(
    reader: &dyn BinaryReader,
    size: u64,
    context: &Context,
) -> Result<Option<&'static str>> {
    let header = reader
        .read(0.0, 32.0, context)
        .await
        .map_err(super::file_error)?;
    if !header.starts_with(PNG) {
        return Ok(detect_supported_image_mime_type(&header));
    }
    if !png(&header) {
        return Ok(None);
    }
    let mut block = Vec::new();
    let mut block_start = 0;
    let mut at = 8u64;
    while at + 8 <= size {
        if at < block_start || at + 8 > block_start + block.len() as u64 {
            block_start = at;
            block = reader
                .read(at as f64, 65536.0, context)
                .await
                .map_err(super::file_error)?;
        }
        let offset = (at - block_start) as usize;
        let chunk = block
            .get(offset..(offset + 8).min(block.len()))
            .unwrap_or_default();
        if ascii(chunk, 4, b"acTL") {
            return Ok(None);
        }
        if ascii(chunk, 4, b"IDAT") {
            return Ok(Some("image/png"));
        }
        let next = at + 12 + u64::from(u32be(chunk, 0));
        if next <= at || next > size {
            break;
        }
        at = next;
    }
    Ok(Some("image/png"))
}
pub fn image_dimensions(b: &[u8], mime: &str) -> Option<ImageDimensions> {
    let (width, height) = match mime {
        "image/png" if b.len() >= 24 => (u32be(b, 16), u32be(b, 20)),
        "image/gif" if b.len() >= 10 => (u16le(b, 6), u16le(b, 8)),
        "image/webp" if b.len() >= 30 => {
            if ascii(b, 12, b"VP8 ") {
                (u16le(b, 26) & 0x3fff, u16le(b, 28) & 0x3fff)
            } else if ascii(b, 12, b"VP8L") {
                let bits = u32le(b, 21);
                ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
            } else if ascii(b, 12, b"VP8X") {
                (
                    u16le(b, 24) + (u32::from(b[26]) << 16) + 1,
                    u16le(b, 27) + (u32::from(b[29]) << 16) + 1,
                )
            } else {
                return None;
            }
        }
        "image/jpeg" => {
            let mut at = 2;
            let mut result = None;
            while at + 9 <= b.len() {
                if b[at] != 255 {
                    return None;
                }
                let marker = b[at + 1];
                if marker == 255 {
                    at += 1;
                    continue;
                }
                if (192..=207).contains(&marker) && !matches!(marker, 196 | 200 | 204) {
                    result = Some((u16be(b, at + 7), u16be(b, at + 5)));
                    break;
                }
                at += 2 + u16be(b, at + 2) as usize;
            }
            result?
        }
        _ => return None,
    };
    Some(ImageDimensions { width, height })
}
