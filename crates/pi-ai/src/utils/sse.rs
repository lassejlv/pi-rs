//! Server-sent events decoding over an HTTP response body (port of the SSE decoder in
//! `api/anthropic-messages.ts`, shared by the HTTP API implementations).

use bytes::Bytes;
use futures::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

/// One decoded server-sent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSentEvent {
    pub event: Option<String>,
    pub data: String,
    pub raw: Vec<String>,
}

#[derive(Debug, Default)]
struct DecoderState {
    event: Option<String>,
    data: Vec<String>,
    raw: Vec<String>,
}

impl DecoderState {
    fn flush(&mut self) -> Option<ServerSentEvent> {
        if self.event.is_none() && self.data.is_empty() {
            return None;
        }
        let event = ServerSentEvent {
            event: self.event.take(),
            data: self.data.join("\n"),
            raw: std::mem::take(&mut self.raw),
        };
        self.data.clear();
        Some(event)
    }

    fn decode_line(&mut self, line: &str) -> Option<ServerSentEvent> {
        if line.is_empty() {
            return self.flush();
        }
        self.raw.push(line.to_string());
        if line.starts_with(':') {
            return None;
        }
        let (field, mut value) = match line.find(':') {
            Some(i) => (&line[..i], &line[i + 1..]),
            None => (line, ""),
        };
        if let Some(stripped) = value.strip_prefix(' ') {
            value = stripped;
        }
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }
}

/// Incremental SSE decoder: feed text, receive complete events.
#[derive(Debug, Default)]
pub struct SseDecoder {
    state: DecoderState,
    buffer: String,
    pending_bytes: Vec<u8>,
}

fn next_line_break(text: &str) -> Option<usize> {
    text.find(['\r', '\n'])
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes (UTF-8, possibly split mid-character).
    pub fn push_bytes(&mut self, bytes: &[u8]) -> Vec<ServerSentEvent> {
        self.pending_bytes.extend_from_slice(bytes);
        let valid_up_to = match std::str::from_utf8(&self.pending_bytes) {
            Ok(_) => self.pending_bytes.len(),
            Err(e) => {
                if e.error_len().is_some() {
                    // Invalid sequence: decode lossily like TextDecoder.
                    let text = String::from_utf8_lossy(&self.pending_bytes).into_owned();
                    self.pending_bytes.clear();
                    return self.push_text(&text);
                }
                e.valid_up_to()
            }
        };
        let rest = self.pending_bytes.split_off(valid_up_to);
        let text = String::from_utf8(std::mem::replace(&mut self.pending_bytes, rest)).unwrap_or_default();
        self.push_text(&text)
    }

    /// Feed decoded text.
    pub fn push_text(&mut self, text: &str) -> Vec<ServerSentEvent> {
        self.buffer.push_str(text);
        let mut events = Vec::new();
        while let Some(index) = next_line_break(&self.buffer) {
            let bytes = self.buffer.as_bytes();
            let mut next = index + 1;
            if bytes[index] == b'\r' {
                if next >= bytes.len() {
                    // A trailing \r may be followed by \n in the next chunk; TS consumes it now
                    // and treats a following \n as an empty line. Mirror that exactly.
                } else if bytes[next] == b'\n' {
                    next += 1;
                }
            }
            let line = self.buffer[..index].to_string();
            self.buffer = self.buffer[next..].to_string();
            if let Some(event) = self.state.decode_line(&line) {
                events.push(event);
            }
        }
        events
    }

    /// Flush at end of stream.
    pub fn finish(&mut self) -> Vec<ServerSentEvent> {
        let mut events = Vec::new();
        if !self.pending_bytes.is_empty() {
            let text = String::from_utf8_lossy(&std::mem::take(&mut self.pending_bytes)).into_owned();
            events.extend(self.push_text(&text));
        }
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            if let Some(event) = self.state.decode_line(&line) {
                events.push(event);
            }
        }
        if let Some(event) = self.state.flush() {
            events.push(event);
        }
        events
    }
}

/// Error raised when the signal aborts while reading the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StreamAborted {
    /// The signal was already aborted when the next read was due (the TS reader loops check
    /// `signal.aborted` and throw `new Error("Request was aborted")`).
    #[error("Request was aborted")]
    BeforeRead,
    /// The signal aborted while a read was pending: the fetch body read rejects with the
    /// `DOMException` `AbortError` of an `abort()` without a reason.
    #[error("This operation was aborted")]
    DuringRead,
}

/// An async SSE event reader over a byte stream.
pub struct SseReader<S> {
    body: S,
    decoder: SseDecoder,
    queue: std::collections::VecDeque<ServerSentEvent>,
    done: bool,
    signal: Option<CancellationToken>,
}

impl<S> SseReader<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    pub fn new(body: S, signal: Option<CancellationToken>) -> Self {
        Self { body, decoder: SseDecoder::new(), queue: Default::default(), done: false, signal }
    }

    /// The next event; `Ok(None)` at end of stream.
    pub async fn next_event(&mut self) -> anyhow::Result<Option<ServerSentEvent>> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Ok(Some(event));
            }
            if self.done {
                return Ok(None);
            }
            if self.signal.as_ref().is_some_and(|s| s.is_cancelled()) {
                return Err(StreamAborted::BeforeRead.into());
            }
            let chunk = match &self.signal {
                Some(signal) => tokio::select! {
                    biased;
                    _ = signal.cancelled() => return Err(StreamAborted::DuringRead.into()),
                    chunk = self.body.next() => chunk,
                },
                None => self.body.next().await,
            };
            match chunk {
                Some(Ok(bytes)) => self.queue.extend(self.decoder.push_bytes(&bytes)),
                Some(Err(error)) => return Err(error.into()),
                None => {
                    self.done = true;
                    self.queue.extend(self.decoder.finish());
                }
            }
        }
    }
}

/// Read SSE events from a reqwest response.
pub fn sse_reader(
    response: reqwest::Response,
    signal: Option<CancellationToken>,
) -> SseReader<impl Stream<Item = Result<Bytes, reqwest::Error>> + Unpin> {
    SseReader::new(Box::pin(crate::utils::wasm::assert_send(response.bytes_stream())), signal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_events_across_chunks_and_line_endings() {
        let mut decoder = SseDecoder::new();
        let mut events = decoder.push_text("event: a\r\ndata: 1\r\n");
        assert!(events.is_empty());
        events.extend(decoder.push_text("\r\n: comment\nevent: b\ndata: x\ndata:y\n\n"));
        events.extend(decoder.push_bytes("data: caf\u{e9}".as_bytes().split_at(8).0));
        events.extend(decoder.push_bytes(&"data: caf\u{e9}".as_bytes()[8..]));
        events.extend(decoder.finish());
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event.as_deref(), Some("a"));
        assert_eq!(events[0].data, "1");
        assert_eq!(events[1].data, "x\ny");
        assert_eq!(events[1].raw, vec![": comment", "event: b", "data: x", "data:y"]);
        assert_eq!(events[2].data, "caf\u{e9}");
    }
}
