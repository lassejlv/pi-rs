//! `@earendil-works/chord` methods: `output.*` (`js/chord/output.js`) over `pi_chord::output`
//! here, and the `context.*`, `json.*` and `delta.*` groups in the submodules.

mod context;

pub use context::context_from;
mod delta;
mod json;
mod sqlite;

use parking_lot::Mutex;
use pi_chord::output::{self, OutputBuffer, OutputLimits, OutputSkip, Retain};

use crate::errors::{ErrorClass, HostError, HostResult};
use crate::host::{Args, Dispatch, Methods};
use crate::value::HostValue;

/// The Chord method group.
pub struct ChordDispatch;

fn size(value: Option<&HostValue>, name: &str) -> HostResult<usize> {
    let n = value
        .and_then(HostValue::as_f64)
        .ok_or_else(|| HostError::type_error(format!("{name} must be a number")))?;
    Ok(if n.is_nan() || n <= 0.0 {
        0
    } else if n >= usize::MAX as f64 {
        usize::MAX
    } else {
        n as usize
    })
}

fn limits(value: &HostValue) -> HostResult<OutputLimits> {
    let retain = match value.get("retain").and_then(HostValue::as_string) {
        Some(s) if s.equals_str("head") => Retain::Head,
        Some(s) if s.equals_str("tail") => Retain::Tail,
        _ => return Err(HostError::type_error("retain must be \"head\" or \"tail\"")),
    };
    Ok(OutputLimits {
        max_bytes: size(value.get("maxBytes"), "maxBytes")?,
        max_lines: size(value.get("maxLines"), "maxLines")?,
        retain,
    })
}

fn skip(value: &HostValue) -> HostResult<Option<OutputSkip>> {
    if value.is_undefined() {
        return Ok(None);
    }
    Ok(Some(OutputSkip {
        bytes: size(value.get("bytes"), "bytes")?,
        newlines: size(value.get("newlines"), "newlines")?,
        ends_with_newline: value
            .get("endsWithNewline")
            .and_then(HostValue::as_bool)
            .unwrap_or(false),
    }))
}

fn buffer(args: &Args) -> HostResult<std::sync::Arc<Mutex<OutputBuffer>>> {
    args.native::<Mutex<OutputBuffer>>(0)
}

impl Dispatch for ChordDispatch {
    fn register(&self, m: &mut Methods) {
        context::register(m);
        json::register(m);
        delta::register(m);
        sqlite::register(m);
        m.sync("output.constants", |_, _| {
            Ok(HostValue::object([(
                "PROGRESS_BYTES_PER_SECOND",
                HostValue::from(output::PROGRESS_BYTES_PER_SECOND),
            )]))
        });
        m.sync("output.sanitize", |_, args| {
            Ok(HostValue::String(output::sanitize_output(args.string(0)?)))
        });
        m.sync("output.utf8ByteLength", |_, args| {
            Ok(HostValue::from(output::utf8_byte_length(args.string(0)?)))
        });
        m.sync("output.characterEnd", |_, args| {
            let bytes = args
                .get(0)
                .as_bytes()
                .ok_or_else(|| HostError::type_error("bytes must be a Uint8Array"))?;
            Ok(HostValue::from(output::character_end(
                bytes,
                size(Some(args.get(1)), "index")?,
            )))
        });
        m.sync("output.bound", |_, args| {
            let slice = output::bound_output(args.string(0)?, limits(args.get(1))?);
            Ok(HostValue::object([
                ("text", HostValue::String(slice.text)),
                ("bytes", HostValue::from(slice.bytes)),
                ("droppedBytes", HostValue::from(slice.dropped_bytes)),
                ("droppedLines", HostValue::from(slice.dropped_lines)),
            ]))
        });
        m.sync("output.bufferNew", |_, args| {
            Ok(HostValue::native(Mutex::new(OutputBuffer::new(limits(
                args.get(0),
            )?))))
        });
        m.sync("output.bufferStoredBytes", |_, args| {
            Ok(HostValue::from(buffer(&args)?.lock().stored_bytes()))
        });
        m.sync("output.bufferPush", |_, args| {
            let buf = buffer(&args)?;
            let skipped = skip(args.get(2))?;
            let accepted = match args.get(1) {
                HostValue::String(text) => buf.lock().push_text(text.clone(), skipped),
                HostValue::Bytes(bytes) => buf.lock().push_bytes(bytes, skipped),
                _ => {
                    return Err(HostError::type_error(
                        "chunk must be a string or a Uint8Array",
                    ));
                }
            };
            accepted
                .map(HostValue::Bool)
                .map_err(|message| HostError::new(ErrorClass::Error, message))
        });
        m.sync("output.bufferEnd", |_, args| {
            buffer(&args)?.lock().end();
            Ok(HostValue::Undefined)
        });
        m.sync("output.bufferSnapshot", |_, args| {
            let snapshot = buffer(&args)?.lock().snapshot();
            Ok(HostValue::object([
                ("text", HostValue::String(snapshot.text)),
                ("droppedBytes", HostValue::from(snapshot.dropped_bytes)),
                ("droppedLines", HostValue::from(snapshot.dropped_lines)),
            ]))
        });
    }
}
