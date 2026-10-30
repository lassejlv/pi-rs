//! Streaming byte offsets and decoded sizes for a selected range of LF lines.
use super::decode::{RangeDecoder, starts_with_bom};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineScan {
    pub newlines: u64,
    pub start: u64,
    pub end: u64,
    pub first_line_end: u64,
    pub last_line_start: u64,
    pub selected_bytes: u64,
    pub first_line_bytes: u64,
}

pub struct LineScanner {
    start_line: u64,
    end_line: Option<u64>,
    position: u64,
    newlines: u64,
    line_start: u64,
    start: Option<u64>,
    end: Option<u64>,
    first_line_end: Option<u64>,
    last_line_start: Option<u64>,
    selected_bytes: u64,
    first_line_bytes: u64,
    selection: Option<RangeDecoder>,
    first_line: Option<RangeDecoder>,
    head: Option<Vec<u8>>,
    bom: bool,
}
impl LineScanner {
    pub fn new(start_line: f64, end_line: Option<f64>) -> Result<Self, &'static str> {
        let end_line = end_line.unwrap_or(f64::INFINITY);
        let valid = |value: f64| {
            value.is_finite()
                && value >= 0.0
                && value.fract() == 0.0
                && value <= 9_007_199_254_740_991.0
        };
        if !valid(start_line)
            || end_line <= start_line
            || !(end_line == f64::INFINITY || valid(end_line))
        {
            return Err("Invalid line range");
        }
        let mut scanner = Self {
            start_line: start_line as u64,
            end_line: (end_line != f64::INFINITY).then_some(end_line as u64),
            position: 0,
            newlines: 0,
            line_start: 0,
            start: None,
            end: None,
            first_line_end: None,
            last_line_start: None,
            selected_bytes: 0,
            first_line_bytes: 0,
            selection: None,
            first_line: None,
            head: Some(Vec::new()),
            bom: false,
        };
        if start_line == 0.0 {
            scanner.begin(0);
        }
        Ok(scanner)
    }
    pub fn push(&mut self, mut chunk: &[u8]) {
        if let Some(head) = &mut self.head {
            let take = (3 - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
            if head.len() < 3 {
                return;
            }
            self.release_head();
            chunk = &chunk[take..];
        }
        self.process(chunk);
    }
    fn release_head(&mut self) {
        let head = self.head.take().unwrap_or_default();
        self.bom = starts_with_bom(&head);
        self.process(&head);
    }
    fn process(&mut self, chunk: &[u8]) {
        let base = self.position;
        let mut from = 0;
        for (index, _) in chunk.iter().enumerate().filter(|(_, byte)| **byte == b'\n') {
            self.feed(chunk, base, from, index);
            let position = base + index as u64;
            if self.newlines == self.start_line {
                self.end_first_line(position);
            }
            if self.end_line == Some(self.newlines + 1) {
                self.end_selection(position);
            }
            self.feed(chunk, base, index, index + 1);
            from = index + 1;
            self.newlines += 1;
            self.line_start = position + 1;
            if self.newlines == self.start_line {
                self.begin(self.line_start);
            }
            if self.end_line == Some(self.newlines + 1) {
                self.last_line_start = Some(self.line_start);
            }
        }
        self.feed(chunk, base, from, chunk.len());
        self.position += chunk.len() as u64;
    }
    pub fn finish(&mut self) -> LineScan {
        if self.head.is_some() {
            self.release_head();
        }
        let size = self.position;
        let Some(start) = self.start else {
            return LineScan {
                newlines: self.newlines,
                start: size,
                end: size,
                first_line_end: size,
                last_line_start: size,
                selected_bytes: 0,
                first_line_bytes: 0,
            };
        };
        if self.first_line_end.is_none() {
            self.end_first_line(size);
        }
        if self.end.is_none() {
            self.end_selection(size);
        }
        LineScan {
            newlines: self.newlines,
            start,
            end: self.end.unwrap_or(size),
            first_line_end: self.first_line_end.unwrap_or(size),
            last_line_start: self.last_line_start.unwrap_or(self.line_start),
            selected_bytes: self.selected_bytes,
            first_line_bytes: self.first_line_bytes,
        }
    }
    fn begin(&mut self, start: u64) {
        self.start = Some(start);
        if self.end_line == Some(self.start_line + 1) {
            self.last_line_start = Some(start);
        }
        self.selection = Some(RangeDecoder::default());
        self.first_line = Some(RangeDecoder::default());
    }
    fn end_first_line(&mut self, position: u64) {
        self.first_line_end = Some(position);
        if let Some(mut decoder) = self.first_line.take() {
            self.first_line_bytes += decoder.decode(&[], true).len() as u64;
        }
    }
    fn end_selection(&mut self, position: u64) {
        self.end = Some(position);
        if let Some(mut decoder) = self.selection.take() {
            self.selected_bytes += decoder.decode(&[], true).len() as u64;
        }
    }
    fn feed(&mut self, chunk: &[u8], base: u64, mut from: usize, to: usize) {
        if self.bom && base + (from as u64) < 3 {
            from = to.min((3 - base) as usize);
        }
        if to <= from {
            return;
        }
        if let Some(decoder) = &mut self.selection {
            self.selected_bytes += decoder.decode(&chunk[from..to], false).len() as u64;
        }
        if let Some(decoder) = &mut self.first_line {
            self.first_line_bytes += decoder.decode(&chunk[from..to], false).len() as u64;
        }
    }
}
