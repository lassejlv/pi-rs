//! Incremental replacement decoding shared by file and output streams.

#[derive(Default)]
pub struct Decoder {
    pending: Vec<u8>,
}
impl Decoder {
    pub fn decode(&mut self, bytes: &[u8], end: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut at = 0;
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending[at..]) {
                Ok(text) => {
                    out.push_str(text);
                    at = self.pending.len();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    out.push_str(
                        std::str::from_utf8(&self.pending[at..at + valid])
                            .expect("validated UTF-8 prefix"),
                    );
                    at += valid;
                    if let Some(len) = error.error_len() {
                        out.push(char::REPLACEMENT_CHARACTER);
                        at += len;
                    } else if end {
                        out.push(char::REPLACEMENT_CHARACTER);
                        at = self.pending.len();
                        break;
                    } else {
                        break;
                    }
                }
            }
        }
        self.pending.drain(..at);
        out
    }
}
