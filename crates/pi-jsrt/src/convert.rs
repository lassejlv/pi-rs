//! Conversion between JS values and `serde_json::Value`.
//!
//! JS → JSON follows `JSON.stringify` semantics (including `toJSON`) with
//! these extensions:
//! - functions become handle markers `{"$jsHandle": <id>}` (see [`crate::JsHandle`]),
//! - `bigint` becomes a decimal string, typed arrays become arrays of numbers,
//!   `Map` becomes an object, `Set` becomes an array,
//! - `Error` objects become `{name, message, stack, ...ownProps}`,
//! - a top-level `undefined` becomes `null`.
//!
//! JSON → JS revives `{"$jsHandle": id}` markers to the stored value and
//! `{"$hostFn": id}` markers to host functions.

use crate::js::{Context, JsResult, JsValue, js_string};
use serde_json::Value;

use crate::engine::{internal, js_str, try_internal};
use crate::error::{self, Error};

/// Marker key for JS values held in the handle table.
pub const HANDLE_KEY: &str = "$jsHandle";
/// Marker key for host functions.
pub const HOST_FN_KEY: &str = "$hostFn";

fn number_to_json(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        if n == 0.0 && n.is_sign_negative() {
            return Value::from(0);
        }
        Value::from(n as i64)
    } else if n.is_finite() {
        serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
    } else {
        Value::Null
    }
}

/// Convert a JS value to JSON (see module docs).
pub fn to_json(value: &JsValue, ctx: &mut Context) -> Result<Value, Error> {
    if value.is_null_or_undefined() {
        return Ok(Value::Null);
    }
    if let Some(b) = value.as_boolean() {
        return Ok(Value::Bool(b));
    }
    if let Some(n) = value.as_number() {
        return Ok(number_to_json(n));
    }
    if let Some(s) = value.as_string() {
        return Ok(Value::String(s.to_std_string_lossy()));
    }
    if let Some(b) = value.as_bigint() {
        return Ok(Value::String(b.to_string()));
    }
    if value.is_symbol() {
        return Ok(Value::Null);
    }
    let helper = internal(ctx).get(js_string!("toHost"), ctx).map_err(|e| error::from_js(e, ctx))?;
    let helper = helper.as_function().ok_or_else(|| Error::host("toHost helper missing"))?;
    let s = helper
        .call(&JsValue::undefined(), std::slice::from_ref(value), ctx)
        .map_err(|e| error::from_js(e, ctx))?;
    match s.as_string() {
        Some(s) => serde_json::from_str(&s.to_std_string_lossy()).map_err(|e| Error::host(format!("invalid JSON from JS: {e}"))),
        None => Ok(Value::Null),
    }
}

fn has_markers(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.contains_key(HANDLE_KEY) || m.contains_key(HOST_FN_KEY) || m.values().any(has_markers),
        Value::Array(a) => a.iter().any(has_markers),
        _ => false,
    }
}

/// Convert JSON to a JS value, reviving handle / host-function markers.
pub fn from_json(value: &Value, ctx: &mut Context) -> JsResult<JsValue> {
    match value {
        Value::Null => Ok(JsValue::null()),
        Value::Bool(b) => Ok(JsValue::from(*b)),
        Value::Number(n) => Ok(JsValue::from(n.as_f64().unwrap_or(f64::NAN))),
        Value::String(s) => Ok(js_str(s)),
        _ if has_markers(value) => {
            let helper = internal(ctx).get(js_string!("fromHost"), ctx)?;
            let helper = helper
                .as_function()
                .ok_or_else(|| crate::engine::type_error("fromHost helper missing"))?;
            helper.call(&JsValue::undefined(), &[js_str(&value.to_string())], ctx)
        }
        _ => JsValue::from_json(value, ctx),
    }
}

/// Human readable description of a value (uses `util.inspect`-like formatting from the prelude).
pub fn describe(value: &JsValue, ctx: &mut Context) -> String {
    let inspect = try_internal(ctx).and_then(|i| i.get(js_string!("inspect"), ctx).ok()).and_then(|f| f.as_function());
    if let Some(f) = inspect
        && let Ok(s) = f.call(&JsValue::undefined(), std::slice::from_ref(value), ctx)
        && let Some(s) = s.as_string()
    {
        return s.to_std_string_lossy();
    }
    value.display().to_string()
}
