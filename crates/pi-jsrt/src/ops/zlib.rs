//! Compression ops backing `node:zlib`: deflate/inflate (zlib, raw, gzip, auto-detect) on
//! zlib-rs (a port of zlib, so output bytes, windowBits/memLevel/strategy/dictionary handling and
//! error messages are zlib's), brotli on the `brotli` crate, zstd on ruzstd, and CRC-32.
//!
//! A stream is an opaque JS object owning a [`ZStream`]. `ops.zlibWrite(stream, input, flush)`
//! consumes all of `input` and returns everything produced so far (the JS glue slices it for
//! callers that pass bounded output buffers, like Node's internal handle API used by ssh2).
//! Errors are thrown as `Error`s with Node's zlib `message`, `code` and `errno`.

use std::io::Read;

use crate::js::{Context, JsArgs, JsResult, JsValue};

use super::{bytes_arg, bytes_to_js, coded_error, native_object, with_native};
use crate::engine::{OpFn, arg_f64, js_str};

// Node's mode numbers (`zlib.constants`).
const DEFLATE: i32 = 1;
const INFLATE: i32 = 2;
const GZIP: i32 = 3;
const GUNZIP: i32 = 4;
const DEFLATERAW: i32 = 5;
const INFLATERAW: i32 = 6;
#[cfg_attr(not(test), allow(dead_code))]
const UNZIP: i32 = 7;
const BROTLI_DECODE: i32 = 8;
const BROTLI_ENCODE: i32 = 9;
const ZSTD_COMPRESS: i32 = 10;
const ZSTD_DECOMPRESS: i32 = 11;

// Flush values.
const Z_NO_FLUSH: i32 = 0;
const Z_PARTIAL_FLUSH: i32 = 1;
const Z_SYNC_FLUSH: i32 = 2;
const Z_FULL_FLUSH: i32 = 3;
const Z_FINISH: i32 = 4;
const Z_BLOCK: i32 = 5;

const CHUNK: usize = 16 * 1024;

/// A zlib error: Node's message, code and errno.
#[derive(Debug, Clone)]
pub(crate) struct ZError {
    pub message: String,
    pub code: String,
    pub errno: i32,
}

impl ZError {
    fn data(message: &str) -> Self {
        Self { message: message.into(), code: "Z_DATA_ERROR".into(), errno: -3 }
    }
    fn eof() -> Self {
        Self { message: "unexpected end of file".into(), code: "Z_BUF_ERROR".into(), errno: -5 }
    }
}

type ZResult<T> = Result<T, ZError>;

// ---------------------------------------------------------------------------
// Deflate / inflate on zlib-rs (a port of zlib: identical output, options and messages)
// ---------------------------------------------------------------------------

/// zlib options of a stream (`windowBits`, `memLevel`, `strategy`, `dictionary`).
#[derive(Clone, Default)]
pub(crate) struct ZOptions {
    pub level: i32,
    pub window_bits: i32,
    pub mem_level: i32,
    pub strategy: i32,
    pub dictionary: Option<Vec<u8>>,
}

fn stream_error(message: &str) -> ZError {
    ZError { message: message.into(), code: "Z_STREAM_ERROR".into(), errno: -2 }
}

/// A zlib deflate stream on zlib-rs's zlib-style API (`z_stream`), which, unlike its safe
/// wrapper, supports `deflateParams` with a strategy exactly like zlib (as Node calls it).
struct RawDeflate {
    strm: Box<zlib_rs::c_api::z_stream>,
}

// SAFETY: the stream owns its state; it is only used from the thread that holds it.
unsafe impl Send for RawDeflate {}

impl RawDeflate {
    fn new(config: zlib_rs::DeflateConfig) -> Option<Self> {
        let mut strm = Box::new(zlib_rs::c_api::z_stream::default());
        (zlib_rs::deflate::init(&mut strm, config) == zlib_rs::ReturnCode::Ok).then_some(Self { strm })
    }

    fn stream(&mut self) -> &mut zlib_rs::deflate::DeflateStream<'static> {
        // SAFETY: `strm` was initialized by `deflate::init` and is ended only in `drop`.
        unsafe { zlib_rs::deflate::DeflateStream::from_stream_mut(&mut *self.strm) }.expect("initialized deflate stream")
    }

    fn message(&self) -> Option<&'static str> {
        if self.strm.msg.is_null() {
            None
        } else {
            // SAFETY: zlib messages are static NUL-terminated strings.
            unsafe { std::ffi::CStr::from_ptr(self.strm.msg) }.to_str().ok()
        }
    }

    /// One `deflate()` call: returns (return code, consumed, produced).
    fn deflate(&mut self, input: &[u8], output: &mut [u8], flush: zlib_rs::DeflateFlush) -> (zlib_rs::ReturnCode, usize, usize) {
        let avail_in = input.len().min(u32::MAX as usize) as u32;
        let avail_out = output.len().min(u32::MAX as usize) as u32;
        self.strm.next_in = input.as_ptr();
        self.strm.avail_in = avail_in;
        self.strm.next_out = output.as_mut_ptr();
        self.strm.avail_out = avail_out;
        let ret = zlib_rs::deflate::deflate(self.stream(), flush);
        let consumed = (avail_in - self.strm.avail_in) as usize;
        let produced = (avail_out - self.strm.avail_out) as usize;
        self.strm.next_in = std::ptr::null();
        self.strm.avail_in = 0;
        self.strm.next_out = std::ptr::null_mut();
        self.strm.avail_out = 0;
        (ret, consumed, produced)
    }

    fn set_dictionary(&mut self, dict: &[u8]) -> bool {
        zlib_rs::deflate::set_dictionary(self.stream(), dict) == zlib_rs::ReturnCode::Ok
    }

    /// `deflateParams(level, strategy)` as Node calls it: after a flush, with no output space.
    fn params(&mut self, level: i32, strategy: i32) -> bool {
        let Ok(strategy) = zlib_rs::Strategy::try_from(strategy) else { return false };
        let mut dummy = [0u8; 1];
        self.strm.next_out = dummy.as_mut_ptr();
        self.strm.avail_out = 0;
        let ret = zlib_rs::deflate::params(self.stream(), level, strategy);
        self.strm.next_out = std::ptr::null_mut();
        matches!(ret, zlib_rs::ReturnCode::Ok | zlib_rs::ReturnCode::BufError)
    }
}

impl Drop for RawDeflate {
    fn drop(&mut self) {
        let _ = zlib_rs::deflate::end(self.stream());
    }
}

struct Deflater {
    z: RawDeflate,
    finished: bool,
}

impl Deflater {
    fn new(mode: i32, o: &ZOptions) -> ZResult<Self> {
        let bits = if o.window_bits == 0 { 15 } else { o.window_bits };
        let window_bits = match mode {
            GZIP => bits + 16,
            DEFLATERAW => -bits,
            _ => bits,
        };
        let config = zlib_rs::DeflateConfig {
            level: o.level,
            method: zlib_rs::Method::Deflated,
            window_bits,
            mem_level: if o.mem_level == 0 { 8 } else { o.mem_level },
            strategy: zlib_rs::Strategy::try_from(o.strategy).map_err(|_| init_failed())?,
        };
        let mut z = RawDeflate::new(config).ok_or_else(init_failed)?;
        if let Some(dict) = &o.dictionary
            && mode != GZIP
            && !z.set_dictionary(dict)
        {
            return Err(ZError { message: "Failed to set dictionary".into(), code: "ERR_ZLIB_INITIALIZATION_FAILED".into(), errno: -2 });
        }
        Ok(Self { z, finished: false })
    }

    fn write(&mut self, input: &[u8], flush: i32, out: &mut Vec<u8>) -> ZResult<()> {
        use zlib_rs::DeflateFlush;
        use zlib_rs::ReturnCode;
        if self.finished {
            return Ok(());
        }
        let flush = match flush {
            Z_NO_FLUSH => DeflateFlush::NoFlush,
            Z_PARTIAL_FLUSH => DeflateFlush::PartialFlush,
            Z_SYNC_FLUSH => DeflateFlush::SyncFlush,
            Z_FULL_FLUSH => DeflateFlush::FullFlush,
            Z_BLOCK => DeflateFlush::Block,
            _ => DeflateFlush::Finish,
        };
        let mut pos = 0;
        loop {
            let start = out.len();
            out.resize(start + CHUNK, 0);
            let (ret, consumed, produced) = self.z.deflate(&input[pos..], &mut out[start..], flush);
            pos += consumed;
            out.truncate(start + produced);
            match ret {
                ReturnCode::StreamEnd => {
                    self.finished = true;
                    return Ok(());
                }
                ReturnCode::Ok | ReturnCode::BufError => {
                    // Done when the input is consumed and the output did not fill up.
                    if pos >= input.len() && produced < CHUNK {
                        return Ok(());
                    }
                }
                _ => return Err(stream_error(self.z.message().unwrap_or("stream error"))),
            }
        }
    }
}

fn init_failed() -> ZError {
    ZError { message: "Initialization failed".into(), code: "ERR_ZLIB_INITIALIZATION_FAILED".into(), errno: -2 }
}

#[derive(Clone, Copy, PartialEq)]
enum InKind {
    Zlib,
    Raw,
    Gzip,
    Auto,
}

struct Inflater {
    kind: InKind,
    window_bits: i32,
    z: Box<zlib_rs::Inflate>,
    dictionary: Option<Vec<u8>>,
    ended: bool,
    /// Gzip: a member ended; more members may follow.
    member_done: bool,
    /// Bytes after a finished gzip member, kept until there is enough to decide.
    pending: Vec<u8>,
    /// Auto mode (unzip) saw a gzip header: behaves like gunzip from then on, as in Node.
    gzip_detected: bool,
    /// Input of the current member (bounded), replayed to recover zlib's error message.
    history: Option<Vec<u8>>,
}

/// zlib-rs reports "repeated call with bad state" for errors detected in its fast decoding
/// loop (where zlib keeps the original message); replaying the input through the slow path
/// recovers zlib's message.
const BAD_STATE: &str = "repeated call with bad state";
const HISTORY_LIMIT: usize = 8 * 1024 * 1024;

impl Inflater {
    fn new(mode: i32, o: &ZOptions) -> ZResult<Self> {
        let kind = match mode {
            INFLATE => InKind::Zlib,
            INFLATERAW => InKind::Raw,
            GUNZIP => InKind::Gzip,
            _ => InKind::Auto,
        };
        let bits = if o.window_bits == 0 && kind == InKind::Raw { 15 } else { o.window_bits };
        let mut this = Self {
            kind,
            window_bits: bits,
            z: Self::make(kind, bits),
            dictionary: o.dictionary.clone(),
            ended: false,
            member_done: false,
            pending: Vec::new(),
            gzip_detected: false,
            history: Some(Vec::new()),
        };
        if kind == InKind::Raw
            && let Some(dict) = this.dictionary.clone()
            && this.z.set_dictionary(&dict).is_err()
        {
            return Err(stream_error("Failed to set dictionary"));
        }
        Ok(this)
    }

    fn make(kind: InKind, bits: i32) -> Box<zlib_rs::Inflate> {
        let bits = bits.clamp(0, 15) as u8;
        Box::new(match kind {
            InKind::Zlib => zlib_rs::Inflate::new(true, bits),
            InKind::Raw => zlib_rs::Inflate::new(false, bits.max(8)),
            InKind::Gzip => zlib_rs::Inflate::new(true, 16 + bits),
            InKind::Auto => zlib_rs::Inflate::new(true, 32 + bits),
        })
    }

    /// The data error of the stream, with zlib's message (see [`BAD_STATE`]).
    fn data_error(&self, rest: &[u8]) -> ZError {
        let message = self.z.error_message().unwrap_or("invalid data");
        if message != BAD_STATE {
            return ZError::data(message);
        }
        let Some(history) = &self.history else { return ZError::data(message) };
        let mut input = history.clone();
        input.extend_from_slice(rest);
        let kind = if self.gzip_detected { InKind::Gzip } else { self.kind };
        let mut z = Self::make(kind, self.window_bits);
        if kind == InKind::Raw
            && let Some(dict) = &self.dictionary
        {
            let _ = z.set_dictionary(dict);
        }
        // Output buffers below 258 bytes keep zlib(-rs) out of its fast loop.
        let mut buf = [0u8; 128];
        let mut pos = 0;
        loop {
            let before_in = z.total_in();
            let before_out = z.total_out();
            match z.decompress(&input[pos..], &mut buf, zlib_rs::InflateFlush::NoFlush) {
                Ok(zlib_rs::Status::StreamEnd) => break,
                Ok(_) => {
                    pos += (z.total_in() - before_in) as usize;
                    if z.total_in() == before_in && z.total_out() == before_out {
                        break;
                    }
                }
                Err(zlib_rs::InflateError::NeedDict { .. }) => match &self.dictionary {
                    Some(dict) if z.set_dictionary(dict).is_ok() => pos += (z.total_in() - before_in) as usize,
                    _ => break,
                },
                Err(_) => {
                    if let Some(m) = z.error_message().filter(|m| *m != BAD_STATE) {
                        return ZError::data(m);
                    }
                    break;
                }
            }
        }
        ZError::data(message)
    }

    fn record(&mut self, consumed: &[u8]) {
        if let Some(h) = &mut self.history {
            if h.len() + consumed.len() > HISTORY_LIMIT {
                self.history = None;
            } else {
                h.extend_from_slice(consumed);
            }
        }
    }

    fn write(&mut self, input: &[u8], flush: i32, out: &mut Vec<u8>) -> ZResult<()> {
        use zlib_rs::{InflateError, InflateFlush, Status};
        let joined: Vec<u8>;
        let mut input = input;
        if !self.pending.is_empty() {
            let mut j = std::mem::take(&mut self.pending);
            j.extend_from_slice(input);
            joined = j;
            input = &joined;
        }
        let mut pos = 0;
        while !self.ended {
            if self.member_done {
                // Like Node: another gzip member may follow; zero padding after one ends the
                // stream, anything else must be a member.
                if pos >= input.len() {
                    break;
                }
                if input[pos] == 0 {
                    self.ended = true;
                    break;
                }
                self.z = Self::make(InKind::Gzip, self.window_bits);
                self.history = Some(Vec::new());
                self.member_done = false;
            }
            if self.kind == InKind::Auto && self.z.total_in() == 0 && pos < input.len() && input[pos] == 0x1f {
                self.gzip_detected = true;
            }
            let start = out.len();
            out.resize(start + CHUNK, 0);
            let before_in = self.z.total_in();
            let before_out = self.z.total_out();
            let r = self.z.decompress(&input[pos..], &mut out[start..], InflateFlush::NoFlush);
            let produced = (self.z.total_out() - before_out) as usize;
            let consumed = (self.z.total_in() - before_in) as usize;
            out.truncate(start + produced);
            if r.is_err() && !matches!(r, Err(InflateError::NeedDict { .. })) {
                return Err(match r {
                    Err(InflateError::DataError) => self.data_error(&input[pos..]),
                    _ => stream_error(self.z.error_message().unwrap_or("stream error")),
                });
            }
            self.record(&input[pos..pos + consumed]);
            pos += consumed;
            match r {
                Ok(Status::StreamEnd) => {
                    if self.kind == InKind::Gzip || self.gzip_detected {
                        self.member_done = true;
                    } else {
                        self.ended = true;
                    }
                }
                Ok(_) => {
                    if produced < CHUNK && (pos >= input.len() || produced == 0) {
                        break;
                    }
                }
                Err(_) => {
                    // Z_NEED_DICT, as in Node: set the dictionary or fail.
                    let Some(dict) = self.dictionary.clone() else {
                        return Err(ZError { message: "Missing dictionary".into(), code: "Z_NEED_DICT".into(), errno: 2 });
                    };
                    if self.z.set_dictionary(&dict).is_err() {
                        return Err(ZError { message: "Bad dictionary".into(), code: "Z_NEED_DICT".into(), errno: 2 });
                    }
                }
            }
        }
        if self.member_done && pos < input.len() {
            self.pending = input[pos..].to_vec();
        }
        if flush == Z_FINISH && !self.ended && !self.member_done {
            return Err(ZError::eof());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Brotli
// ---------------------------------------------------------------------------

type BrotliDecoder = brotli::BrotliState<brotli::HeapAlloc<u8>, brotli::HeapAlloc<u32>, brotli::HeapAlloc<brotli::HuffmanCode>>;
type BrotliEncoder = brotli::enc::encode::BrotliEncoderStateStruct<brotli::enc::StandardAlloc>;

fn brotli_encoder(params: &[(u32, u32)]) -> BrotliEncoder {
    use brotli::enc::encode::BrotliEncoderParameter as P;
    let mut enc = BrotliEncoder::new(brotli::enc::StandardAlloc::default());
    enc.set_parameter(P::BROTLI_PARAM_QUALITY, 11);
    enc.set_parameter(P::BROTLI_PARAM_LGWIN, 22);
    for &(key, value) in params {
        let p = match key {
            0 => P::BROTLI_PARAM_MODE,
            1 => P::BROTLI_PARAM_QUALITY,
            2 => P::BROTLI_PARAM_LGWIN,
            3 => P::BROTLI_PARAM_LGBLOCK,
            4 => P::BROTLI_PARAM_DISABLE_LITERAL_CONTEXT_MODELING,
            5 => P::BROTLI_PARAM_SIZE_HINT,
            6 => P::BROTLI_PARAM_LARGE_WINDOW,
            _ => continue,
        };
        enc.set_parameter(p, value);
    }
    enc
}

fn brotli_encode(enc: &mut BrotliEncoder, input: &[u8], flush: i32, out: &mut Vec<u8>) -> ZResult<()> {
    use brotli::enc::encode::BrotliEncoderOperation as Op;
    let op = match flush {
        1 => Op::BROTLI_OPERATION_FLUSH,
        2 => Op::BROTLI_OPERATION_FINISH,
        _ => Op::BROTLI_OPERATION_PROCESS,
    };
    if enc.is_finished() {
        return Ok(());
    }
    let mut avail_in = input.len();
    let mut in_off = 0;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let mut avail_out = buf.len();
        let mut out_off = 0;
        let ok = enc.compress_stream(op, &mut avail_in, input, &mut in_off, &mut avail_out, &mut buf, &mut out_off, &mut None, &mut |_, _, _, _| ());
        if !ok {
            return Err(ZError { message: "Compression failed".into(), code: "ERR_BROTLI_COMPRESSION_FAILED".into(), errno: -1 });
        }
        out.extend_from_slice(&buf[..out_off]);
        let done = match op {
            Op::BROTLI_OPERATION_FINISH => enc.is_finished(),
            _ => avail_in == 0 && !enc.has_more_output(),
        };
        if done || (out_off == 0 && avail_in == 0 && !enc.has_more_output() && op == Op::BROTLI_OPERATION_PROCESS) {
            return Ok(());
        }
    }
}

struct BrotliDec {
    state: Box<BrotliDecoder>,
    done: bool,
}

fn brotli_decode(dec: &mut BrotliDec, input: &[u8], flush: i32, out: &mut Vec<u8>) -> ZResult<()> {
    if dec.done {
        return Ok(());
    }
    let mut avail_in = input.len();
    let mut in_off = 0;
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0usize;
    loop {
        let mut avail_out = buf.len();
        let mut out_off = 0;
        let r = brotli::BrotliDecompressStream(&mut avail_in, &mut in_off, input, &mut avail_out, &mut out_off, &mut buf, &mut total, &mut dec.state);
        out.extend_from_slice(&buf[..out_off]);
        match r {
            brotli::BrotliResult::ResultSuccess => {
                dec.done = true;
                return Ok(());
            }
            brotli::BrotliResult::NeedsMoreOutput => continue,
            brotli::BrotliResult::NeedsMoreInput => {
                // Node's brotli FINISH on a truncated stream reports the zlib EOF error.
                if flush == 2 {
                    return Err(ZError::eof());
                }
                return Ok(());
            }
            brotli::BrotliResult::ResultFailure => {
                let code = dec.state.error_code;
                let name = format!("{code:?}");
                let short = name.strip_prefix("BROTLI_DECODER").unwrap_or(&name);
                return Err(ZError { message: "Decompression failed".into(), code: format!("ERR_{short}"), errno: code as i32 });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

enum Engine {
    Deflate(Deflater),
    Inflate(Inflater),
    BrotliEnc(Box<BrotliEncoder>),
    BrotliDec(BrotliDec),
    /// zstd (de)compresses the whole input at `finish` (ruzstd has no incremental API here).
    Zstd { compress: bool, input: Vec<u8>, done: bool },
}

pub(crate) struct ZStream {
    mode: i32,
    options: ZOptions,
    params: Vec<(u32, u32)>,
    engine: Engine,
}

impl ZStream {
    pub(crate) fn new(mode: i32, options: ZOptions, params: Vec<(u32, u32)>) -> ZResult<Self> {
        let engine = Self::engine(mode, &options, &params)?;
        Ok(Self { mode, options, params, engine })
    }

    fn engine(mode: i32, options: &ZOptions, params: &[(u32, u32)]) -> ZResult<Engine> {
        Ok(match mode {
            DEFLATE | GZIP | DEFLATERAW => Engine::Deflate(Deflater::new(mode, options)?),
            BROTLI_ENCODE => Engine::BrotliEnc(Box::new(brotli_encoder(params))),
            BROTLI_DECODE => Engine::BrotliDec(BrotliDec {
                state: Box::new(BrotliDecoder::new(Default::default(), Default::default(), Default::default())),
                done: false,
            }),
            ZSTD_COMPRESS | ZSTD_DECOMPRESS => Engine::Zstd { compress: mode == ZSTD_COMPRESS, input: Vec::new(), done: false },
            _ => Engine::Inflate(Inflater::new(mode, options)?),
        })
    }

    pub(crate) fn reset(&mut self) {
        if let Ok(engine) = Self::engine(self.mode, &self.options, &self.params) {
            self.engine = engine;
        }
    }

    /// `params(level, strategy)` on a deflate stream.
    pub(crate) fn params(&mut self, level: i32, strategy: i32) {
        self.options.level = level;
        self.options.strategy = strategy;
        if let Engine::Deflate(d) = &mut self.engine {
            d.z.params(level, strategy);
        }
    }

    /// Consume `input`, appending output. `flush` is a zlib flush value (brotli/zstd: an
    /// operation, 2 = finish).
    pub(crate) fn write(&mut self, input: &[u8], flush: i32) -> ZResult<Vec<u8>> {
        let mut out = Vec::new();
        match &mut self.engine {
            Engine::Deflate(d) => d.write(input, flush, &mut out)?,
            Engine::Inflate(i) => i.write(input, flush, &mut out)?,
            Engine::BrotliEnc(e) => brotli_encode(e, input, flush, &mut out)?,
            Engine::BrotliDec(d) => brotli_decode(d, input, flush, &mut out)?,
            Engine::Zstd { compress, input: buf, done } => {
                if !*done {
                    buf.extend_from_slice(input);
                    if flush == 2 {
                        *done = true;
                        let data = std::mem::take(buf);
                        if *compress {
                            out = ruzstd::encoding::compress_to_vec(&data[..], ruzstd::encoding::CompressionLevel::Fastest);
                        } else {
                            let mut src = &data[..];
                            let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut src).map_err(|e| ZError {
                                message: format!("Decompression failed: {e}"),
                                code: "ZSTD_error_corruption_detected".into(),
                                errno: -20,
                            })?;
                            decoder.read_to_end(&mut out).map_err(|e| ZError {
                                message: format!("Decompression failed: {e}"),
                                code: "ZSTD_error_corruption_detected".into(),
                                errno: -20,
                            })?;
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

/// One-shot helper (tests).
#[cfg(test)]
pub(crate) fn process(mode: i32, level: i32, data: &[u8]) -> ZResult<Vec<u8>> {
    let finish = if matches!(mode, BROTLI_DECODE | BROTLI_ENCODE | ZSTD_COMPRESS | ZSTD_DECOMPRESS) { 2 } else { Z_FINISH };
    ZStream::new(mode, ZOptions { level, window_bits: 15, ..Default::default() }, Vec::new())?.write(data, finish)
}

fn throw(ctx: &mut Context, e: ZError) -> crate::js::JsError {
    coded_error(ctx, "Error", &e.message, &[("code", js_str(&e.code)), ("errno", JsValue::from(e.errno))])
}

/// `ops.zlibNew(mode, level, strategy, brotliParams?: number[], windowBits, memLevel, dictionary)
/// -> stream`
fn op_zlib_new(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let mode = arg_f64(args, 0, ctx)? as i32;
    let level = args.get_or_undefined(1).to_number(ctx).map(|n| if n.is_nan() { -1 } else { n as i32 })?;
    let strategy = args.get_or_undefined(2).to_number(ctx).map(|n| if n.is_nan() { 0 } else { n as i32 })?;
    let num = |v: &JsValue, default: i32, ctx: &mut Context| -> JsResult<i32> {
        if v.is_null_or_undefined() { Ok(default) } else { v.to_number(ctx).map(|n| if n.is_nan() { default } else { n as i32 }) }
    };
    let window_bits = num(args.get_or_undefined(4), 15, ctx)?;
    let mem_level = num(args.get_or_undefined(5), 8, ctx)?;
    let dict = args.get_or_undefined(6);
    let dictionary = if dict.is_null_or_undefined() { None } else { Some(bytes_arg(dict, ctx)?) };
    let mut params = Vec::new();
    if let Some(obj) = args.get_or_undefined(3).as_object() {
        let len = obj.get(crate::js::js_string!("length"), ctx)?.to_number(ctx)? as u32;
        let mut i = 0;
        while i + 1 < len {
            let k = obj.get(i, ctx)?.to_number(ctx)? as u32;
            let v = obj.get(i + 1, ctx)?.to_number(ctx)? as u32;
            params.push((k, v));
            i += 2;
        }
    }
    if !(1..=11).contains(&mode) {
        return Err(crate::engine::type_error(format!("Bad argument: mode {mode}")));
    }
    let options = ZOptions { level, window_bits, mem_level, strategy, dictionary };
    match ZStream::new(mode, options, params) {
        Ok(s) => Ok(native_object(s)),
        Err(e) => {
            let err = coded_error(ctx, "Error", "Initialization failed", &[("code", js_str("ERR_ZLIB_INITIALIZATION_FAILED"))]);
            let _ = e;
            Err(err)
        }
    }
}

/// `ops.zlibParams(stream, level, strategy)`
fn op_zlib_params(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let level = arg_f64(args, 1, ctx)? as i32;
    let strategy = arg_f64(args, 2, ctx)? as i32;
    with_native::<ZStream, _>(args.get_or_undefined(0), |s| s.params(level, strategy))?;
    Ok(JsValue::undefined())
}

/// `ops.zlibWrite(stream, input, flush) -> Uint8Array`
fn op_zlib_write(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let input = bytes_arg(args.get_or_undefined(1), ctx)?;
    let flush = arg_f64(args, 2, ctx)? as i32;
    let result = with_native::<ZStream, _>(args.get_or_undefined(0), |s| s.write(&input, flush))?;
    match result {
        Ok(out) => bytes_to_js(&out, ctx),
        Err(e) => Err(throw(ctx, e)),
    }
}

fn op_zlib_reset(_: &JsValue, args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_native::<ZStream, _>(args.get_or_undefined(0), |s| s.reset())?;
    Ok(JsValue::undefined())
}

/// `ops.crc32(data, initial) -> number`
fn op_crc32(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let data = bytes_arg(args.get_or_undefined(0), ctx)?;
    let init = args.get_or_undefined(1).to_number(ctx)?;
    let init = if init.is_finite() { init as u32 } else { 0 };
    let mut h = crc32fast::Hasher::new_with_initial(init);
    h.update(&data);
    Ok(JsValue::from(h.finalize()))
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("zlibNew", 7, op_zlib_new),
    ("zlibParams", 3, op_zlib_params),
    ("zlibWrite", 3, op_zlib_write),
    ("zlibReset", 1, op_zlib_reset),
    ("crc32", 2, op_crc32),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deflate_round_trips_and_matches_zlib_bytes() {
        // Node: zlib.deflateSync("abc").toString("hex")
        assert_eq!(hex(&process(DEFLATE, -1, b"abc").unwrap()), "789c4b4c4a0600024d0127");
        // Node on macOS uses the Darwin OS byte (0x13); the Linux fixture uses 0x03.
        let gzip_hex=if cfg!(target_os="macos") { "1f8b08000000000000134b4c4a0600c241243503000000" } else { "1f8b08000000000000034b4c4a0600c241243503000000" };
        assert_eq!(hex(&process(GZIP, -1, b"abc").unwrap()), gzip_hex);
        let gzip_level9_hex=if cfg!(target_os="macos") { "1f8b08000000000002134b4c4a0600c241243503000000" } else { "1f8b08000000000002034b4c4a0600c241243503000000" };
        assert_eq!(hex(&process(GZIP, 9, b"abc").unwrap()), gzip_level9_hex);
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8 ^ (i / 1000) as u8).collect();
        for (c, d) in [(DEFLATE, INFLATE), (GZIP, GUNZIP), (DEFLATERAW, INFLATERAW), (GZIP, UNZIP), (DEFLATE, UNZIP), (BROTLI_ENCODE, BROTLI_DECODE), (ZSTD_COMPRESS, ZSTD_DECOMPRESS)] {
            let z = process(c, -1, &big).unwrap();
            assert!(z.len() < big.len(), "mode {c} did not compress");
            assert_eq!(process(d, -1, &z).unwrap(), big, "mode {c}->{d}");
        }
    }

    #[test]
    fn inflate_streams_byte_by_byte() {
        let z = process(GZIP, -1, b"hello hello hello world").unwrap();
        let mut s = ZStream::new(GUNZIP, ZOptions { level: -1, window_bits: 15, ..Default::default() }, vec![]).unwrap();
        let mut out = Vec::new();
        for b in &z {
            out.extend(s.write(&[*b], Z_NO_FLUSH).unwrap());
        }
        out.extend(s.write(&[], Z_FINISH).unwrap());
        assert_eq!(out, b"hello hello hello world");
    }

    #[test]
    fn errors_match_node() {
        let msg = |mode, data: &[u8]| process(mode, -1, data).unwrap_err().message;
        assert_eq!(msg(INFLATE, b"hello"), "incorrect header check");
        assert_eq!(msg(GUNZIP, b"hello world"), "incorrect header check");
        let z = process(DEFLATE, -1, b"abc").unwrap();
        let e = process(INFLATE, -1, &z[..5]).unwrap_err();
        assert_eq!((e.message.as_str(), e.code.as_str(), e.errno), ("unexpected end of file", "Z_BUF_ERROR", -5));
        let mut bad = z.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(msg(INFLATE, &bad), "incorrect data check");
        let mut g = process(GZIP, -1, b"abc").unwrap();
        let n = g.len();
        g[n - 1] ^= 1;
        assert_eq!(msg(GUNZIP, &g), "incorrect length check");
        g[n - 1] ^= 1;
        g[n - 5] ^= 1;
        assert_eq!(msg(GUNZIP, &g), "incorrect data check");
        assert_eq!(msg(INFLATERAW, &[0xff, 0xff, 0xff]), "invalid block type");
        let e = process(BROTLI_DECODE, -1, b"garbage").unwrap_err();
        // Node (C brotli) reports PADDING_1 (-14) here; the Rust decoder notices PADDING_2.
        assert_eq!(e.message, "Decompression failed");
        assert!(e.code.starts_with("ERR__ERROR_FORMAT_") && e.errno < 0, "{e:?}");
    }

    #[test]
    fn params_switches_level_mid_stream() {
        let mut s = ZStream::new(DEFLATE, ZOptions { level: -1, window_bits: 15, mem_level: 8, ..Default::default() }, vec![]).unwrap();
        let mut out = s.write(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", Z_NO_FLUSH).unwrap();
        out.extend(s.write(b"", Z_SYNC_FLUSH).unwrap());
        s.params(0, 0);
        out.extend(s.write(b"bbbbbbbbbbbbbbbbbbbbbbbbbbbb", Z_FINISH).unwrap());
        // Node 22: level 0 after a sync flush switches to stored blocks.
        assert_eq!(hex(&out), "789c4a4cc40b00000000ffff011c00e3ff62626262626262626262626262626262626262626262626262626262a0991678");
    }

    #[test]
    fn gzip_members_and_trailing_data() {
        let mut two = process(GZIP, -1, b"abc").unwrap();
        two.extend(process(GZIP, -1, b"def").unwrap());
        assert_eq!(process(GUNZIP, -1, &two).unwrap(), b"abcdef");
        let mut garbage = process(GZIP, -1, b"abc").unwrap();
        garbage.extend_from_slice(b"garbage");
        assert_eq!(process(GUNZIP, -1, &garbage).unwrap_err().message, "incorrect header check");
        let mut padded = process(GZIP, -1, b"abc").unwrap();
        padded.extend_from_slice(&[0; 512]);
        assert_eq!(process(GUNZIP, -1, &padded).unwrap(), b"abc");
        let mut zlib_trailing = process(DEFLATE, -1, b"abc").unwrap();
        zlib_trailing.extend_from_slice(b"garbage");
        assert_eq!(process(INFLATE, -1, &zlib_trailing).unwrap(), b"abc");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
