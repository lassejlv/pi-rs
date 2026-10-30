//! Error types and JS <-> Rust error mapping.

use std::fmt;

use crate::js::{Context, JsError, JsNativeError, JsObject, JsValue, js_string};

/// A JavaScript exception captured on the JS thread.
#[derive(Debug, Clone, PartialEq)]
pub struct JsException {
    /// `error.name` (e.g. `TypeError`), or `"Error"` for non-object throws.
    pub name: String,
    /// `error.message`, or the stringified thrown value.
    pub message: String,
    /// `error.stack` (Node-style: `Name: message\n    at ...`), source-mapped for TypeScript.
    pub stack: Option<String>,
    /// `error.code` if present (Node system errors, e.g. `ENOENT`).
    pub code: Option<String>,
}

impl fmt::Display for JsException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.name.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.name, self.message)
        }
    }
}

/// Errors returned by the runtime API.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// A JavaScript exception (thrown or a rejected promise).
    #[error("{0}")]
    Js(JsException),
    /// A parse / transpile error.
    #[error("SyntaxError: {message}")]
    Syntax {
        /// File (or virtual name) that failed to parse.
        file: String,
        /// Diagnostic message(s) with locations.
        message: String,
    },
    /// Module resolution failed.
    #[error("Cannot find module '{specifier}' imported from {referrer}")]
    ModuleNotFound {
        /// The requested specifier.
        specifier: String,
        /// The importing file or directory.
        referrer: String,
    },
    /// An error raised by Rust host code. Surfaces in JS as `Error(message)`.
    #[error("{0}")]
    Host(String),
    /// A handle id that is unknown (already released, or from another runtime).
    #[error("invalid JS handle {0}")]
    InvalidHandle(u64),
    /// `call_blocking` (or another blocking API) was used on the JS thread itself.
    #[error("blocking call into the JS runtime from its own thread would deadlock")]
    Deadlock,
    /// The runtime thread has stopped.
    #[error("JS runtime has shut down")]
    Terminated,
}

impl Error {
    /// Build a host error from any message.
    pub fn host(msg: impl Into<String>) -> Self {
        Error::Host(msg.into())
    }

    /// Build a JS exception value with a specific error class name.
    pub fn js(name: impl Into<String>, message: impl Into<String>) -> Self {
        Error::Js(JsException {
            name: name.into(),
            message: message.into(),
            stack: None,
            code: None,
        })
    }

    /// The JS exception if this is one.
    pub fn as_js(&self) -> Option<&JsException> {
        match self {
            Error::Js(e) => Some(e),
            _ => None,
        }
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast::<Error>() {
            Ok(e) => e,
            Err(e) => Error::Host(format!("{e:#}")),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Host(e.to_string())
    }
}

/// Result alias for the runtime API.
pub type Result<T, E = Error> = std::result::Result<T, E>;

fn get_string(obj: &JsObject, key: &str, ctx: &mut Context) -> Option<String> {
    let v = obj.get(js_string!(key), ctx).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    v.to_string(ctx).ok().map(|s| s.to_std_string_lossy())
}

/// Convert a thrown JS value into an [`Error`].
pub(crate) fn from_thrown(value: &JsValue, ctx: &mut Context) -> Error {
    if value.is_uncatchable_error() {
        // QuickJS's interrupt handler stopped a runaway script (`loop_iteration_limit`).
        return Error::js("RuntimeLimitError", "Maximum loop iteration limit exceeded: the script was interrupted");
    }
    if let Some(obj) = value.as_object() {
        let name = get_string(&obj, "name", ctx).unwrap_or_else(|| "Error".into());
        let message = get_string(&obj, "message", ctx).unwrap_or_default();
        let stack = get_string(&obj, "stack", ctx).map(|s| crate::engine::remap_stack(ctx, &s));
        let code = get_string(&obj, "code", ctx);
        if name == "Error" && message.is_empty() && stack.is_none() {
            // Not an Error-like object: describe it.
            let message = crate::convert::describe(value, ctx);
            return Error::Js(JsException {
                name: String::new(),
                message,
                stack: None,
                code: None,
            });
        }
        return Error::Js(JsException {
            name,
            message,
            stack,
            code,
        });
    }
    let message = value
        .to_string(ctx)
        .map(|s| s.to_std_string_lossy())
        .unwrap_or_else(|_| value.display().to_string());
    Error::Js(JsException {
        name: String::new(),
        message,
        stack: None,
        code: None,
    })
}

/// Convert a JS error into an [`Error`].
pub(crate) fn from_js(err: JsError, ctx: &mut Context) -> Error {
    match err.into_opaque(ctx) {
        Ok(v) => from_thrown(&v, ctx),
        Err(e) => Error::Host(e.to_string()),
    }
}

/// Convert an [`Error`] into a JS value suitable for throwing / rejecting.
pub(crate) fn to_js_error(err: &Error, ctx: &mut Context) -> JsValue {
    let (name, message, code, stack) = match err {
        Error::Js(e) => (e.name.clone(), e.message.clone(), e.code.clone(), e.stack.clone()),
        Error::Syntax { .. } => ("SyntaxError".to_string(), err.to_string().trim_start_matches("SyntaxError: ").to_string(), None, None),
        Error::ModuleNotFound { .. } => ("Error".to_string(), err.to_string(), Some("ERR_MODULE_NOT_FOUND".to_string()), None),
        _ => ("Error".to_string(), err.to_string(), None, None),
    };
    let native = match name.as_str() {
        "TypeError" => JsNativeError::typ(),
        "RangeError" => JsNativeError::range(),
        "SyntaxError" => JsNativeError::syntax(),
        "ReferenceError" => JsNativeError::reference(),
        "EvalError" => JsNativeError::eval(),
        "URIError" => JsNativeError::uri(),
        _ => JsNativeError::error(),
    };
    let obj = native.with_message(message).to_opaque(ctx);
    if !matches!(
        name.as_str(),
        "Error" | "TypeError" | "RangeError" | "SyntaxError" | "ReferenceError" | "EvalError" | "URIError" | ""
    ) {
        let _ = obj.set(js_string!("name"), js_string!(name.as_str()), false, ctx);
    }
    if let Some(code) = code {
        let _ = obj.set(js_string!("code"), js_string!(code.as_str()), false, ctx);
    }
    if let Some(stack) = stack {
        let _ = obj.set(js_string!("stack"), js_string!(stack.as_str()), false, ctx);
    }
    obj.into()
}

/// Convert an [`Error`] into a throwable JS error.
pub(crate) fn to_js(err: &Error, ctx: &mut Context) -> JsError {
    JsError::from_opaque(to_js_error(err, ctx))
}
