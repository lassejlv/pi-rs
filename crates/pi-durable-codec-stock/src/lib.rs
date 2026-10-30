//! Test transport only. Both targets compile the production codec source files.
#![cfg(target_arch = "wasm32")]
#[path = "../../pi-durable/src/env/decode.rs"]
mod decode;
#[path = "../../pi-durable/src/env/line_scan.rs"]
mod line_scan;
use std::sync::{Mutex, OnceLock};

#[derive(Default)]
struct State {
    input: Vec<u8>,
    output: Vec<u8>,
    decoders: Vec<Decoder>,
    scanners: Vec<line_scan::LineScanner>,
}
enum Decoder {
    Stream(decode::StreamDecoder),
    Range(decode::RangeDecoder),
}
fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}
#[unsafe(no_mangle)]
pub extern "C" fn input_buffer(length: u32) -> *mut u8 {
    let mut state = state().lock().unwrap();
    state.input.resize(length as usize, 0);
    state.input.as_mut_ptr()
}
#[unsafe(no_mangle)]
pub extern "C" fn output_buffer() -> *const u8 {
    state().lock().unwrap().output.as_ptr()
}
#[unsafe(no_mangle)]
pub extern "C" fn starts_bom() -> u32 {
    u32::from(decode::starts_with_bom(&state().lock().unwrap().input))
}
#[unsafe(no_mangle)]
pub extern "C" fn decoder_create(range: u32) -> u32 {
    let mut state = state().lock().unwrap();
    let id = state.decoders.len();
    state.decoders.push(if range == 0 {
        Decoder::Stream(decode::StreamDecoder::default())
    } else {
        Decoder::Range(decode::RangeDecoder::default())
    });
    id as u32
}
#[unsafe(no_mangle)]
pub extern "C" fn decoder_decode(id: u32, end: u32) -> u32 {
    let mut state = state().lock().unwrap();
    let State {
        input,
        output,
        decoders,
        ..
    } = &mut *state;
    *output = match &mut decoders[id as usize] {
        Decoder::Stream(decoder) => decoder.decode((end == 0).then_some(input.as_slice())),
        Decoder::Range(decoder) => decoder.decode(input, end != 0),
    }
    .into_bytes();
    output.len() as u32
}
#[unsafe(no_mangle)]
pub extern "C" fn scanner_create(start: f64, end: f64) -> i32 {
    let mut state = state().lock().unwrap();
    let Ok(scanner) = line_scan::LineScanner::new(start, Some(end)) else {
        return -1;
    };
    let id = state.scanners.len();
    state.scanners.push(scanner);
    id as i32
}
#[unsafe(no_mangle)]
pub extern "C" fn scanner_push(id: u32) {
    let mut state = state().lock().unwrap();
    let State {
        input, scanners, ..
    } = &mut *state;
    scanners[id as usize].push(input);
}
#[unsafe(no_mangle)]
pub extern "C" fn scanner_finish(id: u32) -> u32 {
    let mut state = state().lock().unwrap();
    state.output = serde_json::to_vec(&state.scanners[id as usize].finish()).unwrap();
    state.output.len() as u32
}
