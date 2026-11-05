//! Whole-stream UTF-8 replacement decoding with one leading BOM removed.
pub use pi_json::utf8::Decoder as RangeDecoder;

pub fn starts_with_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xef, 0xbb, 0xbf])
}

#[derive(Default)]
pub struct StreamDecoder {
    decoder: RangeDecoder,
    started: bool,
}
impl StreamDecoder {
    pub fn decode(&mut self, bytes: Option<&[u8]>) -> String {
        let text = self
            .decoder
            .decode(bytes.unwrap_or_default(), bytes.is_none());
        if self.started || text.is_empty() {
            return text;
        }
        self.started = true;
        text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned()
    }
}
