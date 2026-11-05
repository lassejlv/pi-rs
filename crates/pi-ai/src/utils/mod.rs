//! Utilities (port of `utils/*`).

pub mod abort;
pub mod diagnostics;
pub mod error_body;
pub mod estimate;
pub mod event_stream;
pub mod hash;
pub mod headers;
pub mod http;
pub mod identity;
pub mod json_parse;
/// `JSON.parse` with V8's `SyntaxError` messages (the `pi-json` crate).
pub use pi_json as js_json;
pub mod model_operations;
pub mod models_error;
pub mod node_http_proxy;
pub mod oauth_page;
pub mod overflow;
pub mod provider_env;
pub mod provider_retry;
pub mod retry;
pub mod sanitize_unicode;
pub mod sleep;
pub mod sse;
pub mod text;
pub mod transcript;
pub mod typebox_helpers;
pub mod uuid;
pub mod validation;
pub mod version;
pub mod wasm;

/// Current Unix time in milliseconds (`Date.now()`).
pub fn now_ms() -> i64 {
    // `SystemTime::now` panics on wasm32-unknown-unknown.
    #[cfg(target_arch = "wasm32")]
    return js_sys::Date::now() as i64;
    #[cfg(not(target_arch = "wasm32"))]
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// JavaScript string length (UTF-16 code units).
pub fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}
