//! Chord contracts used by the native durable runtime.
//!
//! Reference: Chord 1.4.0 at `5e63241d0294ed3fcfc4388c737a8ea27e09c74e`.

pub mod context;
pub mod delta;
pub mod json;
pub mod output;
pub mod rt;
pub mod sqlite;
pub mod watch;

pub use context::{AbortController, AbortSignal, Context};
pub use json::{JsonString, Value as JsonValue};
pub use watch::{NativeWatch, Watch, WatchEnd, WatchError};

/// An independent strict-JSON tree. Rust's JSON type excludes cycles, sparse arrays,
/// accessors, undefined and non-finite numbers at construction.
pub fn copy_json(value: &JsonValue) -> JsonValue {
    value.clone()
}
