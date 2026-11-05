//! Bounded head/tail output with exact dropped totals and incremental UTF-8 decoding.

mod progress;
pub use progress::{
    Progress, ProgressClock, ProgressCompletion, ProgressWaiter, ProgressWithError,
    SystemProgressClock,
};

use crate::JsonString;
use pi_json::utf8::Decoder;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Retain {
    Head,
    Tail,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputLimits {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub retain: Retain,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundedOutput {
    pub text: JsonString,
    pub dropped_bytes: usize,
    pub dropped_lines: usize,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputSlice {
    pub text: JsonString,
    pub bytes: usize,
    pub dropped_bytes: usize,
    pub dropped_lines: usize,
}
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputSkip {
    pub bytes: usize,
    pub newlines: usize,
    pub ends_with_newline: bool,
}

pub fn sanitize_output(text: &JsonString) -> JsonString {
    JsonString::from_utf16(
        text.units()
            .iter()
            .copied()
            .filter(|u| !matches!(u,0..=8|11..=31|0xfff9..=0xfffb))
            .collect(),
    )
}
pub fn utf8_byte_length(text: &JsonString) -> usize {
    char::decode_utf16(text.units().iter().copied())
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER).len_utf8())
        .sum()
}
pub fn character_end(bytes: &[u8], index: usize) -> usize {
    let mut end = index;
    while end > 0 && bytes.get(end).is_some_and(|b| b & 0xc0 == 0x80) {
        end -= 1;
    }
    end
}
fn character_start(bytes: &[u8], index: usize) -> usize {
    let mut start = index.min(bytes.len());
    while start < bytes.len() && bytes[start] & 0xc0 == 0x80 {
        start += 1;
    }
    start
}
fn line_count(bytes: &[u8]) -> usize {
    bytes.iter().filter(|b| **b == b'\n').count()
        + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n'))
}
fn newlines(text: &JsonString) -> usize {
    text.units().iter().filter(|u| **u == 10).count()
}
fn terminated(text: &JsonString) -> bool {
    text.units().is_empty() || text.units().last() == Some(&10)
}

pub fn bound_output(text: &JsonString, limits: OutputLimits) -> OutputSlice {
    let bytes = text.to_string_lossy().into_bytes();
    let (mut from, mut to) = (0, bytes.len());
    if limits.max_bytes == 0 || limits.max_lines == 0 {
        to = 0;
    } else {
        match limits.retain {
            Retain::Head => {
                let mut lines = 0;
                for (index, b) in bytes.iter().enumerate() {
                    if *b == b'\n' {
                        lines += 1;
                        if lines == limits.max_lines {
                            to = index + 1;
                            break;
                        }
                    }
                }
                if to > limits.max_bytes {
                    to = bytes[..limits.max_bytes.min(bytes.len())]
                        .iter()
                        .rposition(|b| *b == b'\n')
                        .map_or_else(|| character_end(&bytes, limits.max_bytes), |i| i + 1);
                }
            }
            Retain::Tail => {
                let last = if bytes.last() == Some(&b'\n') {
                    bytes.len().saturating_sub(1)
                } else {
                    bytes.len()
                };
                let mut lines = 1;
                for index in (0..last).rev() {
                    if bytes[index] == b'\n' {
                        if lines == limits.max_lines {
                            from = index + 1;
                            break;
                        }
                        lines += 1;
                    }
                }
                if bytes.len() - from > limits.max_bytes {
                    let window = bytes.len() - limits.max_bytes;
                    from = bytes[window - 1..]
                        .iter()
                        .position(|b| *b == b'\n')
                        .map(|i| window - 1 + i)
                        .filter(|i| *i + 1 < bytes.len())
                        .map_or_else(|| character_start(&bytes, window), |i| i + 1);
                }
            }
        }
    }
    let kept = &bytes[from..to];
    OutputSlice {
        text: if kept.len() == bytes.len() {
            text.clone()
        } else {
            String::from_utf8(kept.to_vec())
                .expect("slice ends on UTF-8 boundaries")
                .into()
        },
        bytes: kept.len(),
        dropped_bytes: bytes.len() - kept.len(),
        dropped_lines: line_count(&bytes) - line_count(kept),
    }
}

fn tail_margin(text: &JsonString, limits: OutputLimits) -> JsonString {
    let bytes = text.to_string_lossy().into_bytes();
    let byte_start = if bytes.len() > limits.max_bytes {
        character_end(&bytes, bytes.len() - limits.max_bytes - 1)
    } else {
        0
    };
    let mut line_start = 0;
    let mut lines = 0;
    for index in (0..bytes.len()).rev() {
        if bytes[index] == b'\n' {
            lines += 1;
            if lines > limits.max_lines {
                line_start = index;
                break;
            }
        }
    }
    let start = byte_start.max(line_start);
    if start == 0 {
        text.clone()
    } else {
        String::from_utf8(bytes[start..].to_vec())
            .expect("tail margin UTF-8 boundary")
            .into()
    }
}

struct Chunk {
    text: JsonString,
    bytes: usize,
    newlines: usize,
}
pub struct OutputBuffer {
    limits: OutputLimits,
    decoder: Decoder,
    started: bool,
    chunks: VecDeque<Chunk>,
    stored_bytes: usize,
    stored_newlines: usize,
    full: bool,
    total_bytes: usize,
    total_newlines: usize,
    ends_with_newline: bool,
}
impl OutputBuffer {
    pub fn new(limits: OutputLimits) -> Self {
        Self {
            limits,
            decoder: Decoder::default(),
            started: false,
            chunks: VecDeque::new(),
            stored_bytes: 0,
            stored_newlines: 0,
            full: false,
            total_bytes: 0,
            total_newlines: 0,
            ends_with_newline: true,
        }
    }
    pub fn stored_bytes(&self) -> usize {
        self.stored_bytes
    }
    pub fn push_text(
        &mut self,
        text: impl Into<JsonString>,
        skipped: Option<OutputSkip>,
    ) -> Result<bool, String> {
        let pending: JsonString = self.decoder.decode(&[], true).into();
        self.push_decoded(pending, text.into(), false, skipped)
    }
    pub fn push_bytes(
        &mut self,
        bytes: &[u8],
        skipped: Option<OutputSkip>,
    ) -> Result<bool, String> {
        let pending: JsonString = if skipped.is_some() {
            self.decoder.decode(&[], true)
        } else {
            String::new()
        }
        .into();
        let text: JsonString = self.decoder.decode(bytes, false).into();
        self.push_decoded(pending, text, true, skipped)
    }
    fn push_decoded(
        &mut self,
        mut pending: JsonString,
        mut text: JsonString,
        bytes: bool,
        skipped: Option<OutputSkip>,
    ) -> Result<bool, String> {
        let first = !self.started && pending.units().is_empty() && skipped.is_none();
        if !pending.units().is_empty() || !text.units().is_empty() || skipped.is_some() {
            self.started = true;
        }
        if first && bytes && text.units().first() == Some(&0xfeff) {
            text.truncate_front(1);
        }
        if let Some(skipped) = skipped {
            if self.limits.retain != Retain::Tail {
                return Err("Skipped output requires tail retention".into());
            }
            self.accept(pending);
            if skipped.bytes > 0 {
                self.total_bytes += skipped.bytes;
                self.total_newlines += skipped.newlines;
                self.ends_with_newline = skipped.ends_with_newline;
                self.chunks.clear();
                self.stored_bytes = 0;
                self.stored_newlines = 0;
            }
            self.accept(text);
            Ok(true)
        } else {
            pending.append(&text);
            Ok(self.accept(pending))
        }
    }
    pub fn end(&mut self) {
        let text = self.decoder.decode(&[], true);
        self.accept(text.into());
    }
    fn accept(&mut self, text: JsonString) -> bool {
        if text.units().is_empty() {
            return false;
        }
        let bytes = utf8_byte_length(&text);
        let newlines = newlines(&text);
        self.total_bytes += bytes;
        self.total_newlines += newlines;
        self.ends_with_newline = terminated(&text);
        if self.full {
            return true;
        }
        self.stored_bytes += bytes;
        self.stored_newlines += newlines;
        self.chunks.push_back(Chunk {
            text,
            bytes,
            newlines,
        });
        if self.limits.retain == Retain::Head {
            self.full = self.stored_bytes > self.limits.max_bytes
                || self.stored_newlines >= self.limits.max_lines;
            return true;
        }
        while self.chunks.len() > 1 {
            let first = self.chunks.front().unwrap();
            let bytes_after = self.stored_bytes - first.bytes;
            let newlines_after = self.stored_newlines - first.newlines;
            if bytes_after <= self.limits.max_bytes.saturating_add(1)
                && newlines_after <= self.limits.max_lines.saturating_add(1)
            {
                break;
            }
            self.chunks.pop_front();
            self.stored_bytes = bytes_after;
            self.stored_newlines = newlines_after;
        }
        true
    }
    pub fn snapshot(&mut self) -> BoundedOutput {
        let stored = JsonString::from_utf16(
            self.chunks
                .iter()
                .flat_map(|c| c.text.units().iter().copied())
                .collect(),
        );
        let kept = bound_output(&stored, self.limits);
        let stored_lines = self.stored_newlines + usize::from(!terminated(&stored));
        let kept_lines = stored_lines - kept.dropped_lines;
        if self.limits.retain == Retain::Tail || self.chunks.len() > 1 {
            let text = if self.limits.retain == Retain::Tail {
                tail_margin(&stored, self.limits)
            } else {
                stored
            };
            let bytes = if self.limits.retain == Retain::Tail {
                utf8_byte_length(&text)
            } else {
                self.stored_bytes
            };
            self.stored_bytes = bytes;
            self.stored_newlines = newlines(&text);
            self.chunks.clear();
            if !text.units().is_empty() {
                self.chunks.push_back(Chunk {
                    text,
                    bytes,
                    newlines: self.stored_newlines,
                });
            }
        }
        BoundedOutput {
            text: sanitize_output(&kept.text),
            dropped_bytes: self.total_bytes - kept.bytes,
            dropped_lines: self.total_newlines + usize::from(!self.ends_with_newline) - kept_lines,
        }
    }
}

pub const PROGRESS_BYTES_PER_SECOND: usize = 100 * 1024;
