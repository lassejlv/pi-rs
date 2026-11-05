//! Values crossing the embedded host boundary, converted directly between QuickJS [`JsValue`]s and
//! [`HostValue`]s without `serde_json` (whose strings cannot hold lone UTF-16 surrogates).
//!
//! JS → host:
//! - `undefined`, `null`, booleans and numbers (including `-0`, `NaN`, `±Infinity`) are kept
//!   exactly; strings keep their UTF-16 code units ([`JsonString`]);
//! - arrays become [`HostValue::Array`] (holes read as `undefined`), `Uint8Array`s (and
//!   subclasses such as `Buffer`) become [`HostValue::Bytes`];
//! - plain objects (prototype `Object.prototype` or `null`) become [`HostValue::Object`] with
//!   their own enumerable string keys in ECMAScript order, read with `[[Get]]` (getters run);
//! - functions, symbols, class instances and every other object become [`HostValue::Handle`]
//!   (the same JS value comes back on return), as do values wrapped with `__piDurable.ref(v)`;
//! - objects made by the host for a [`NativeRef`] come back as [`HostValue::Native`];
//! - `bigint` and cyclic structures are rejected with a `TypeError`.
//!
//! Host → JS is the inverse: objects are created with `Object.prototype` and data properties
//! (so `"__proto__"` is an ordinary key), handles revive their original value.
//!
//! [`HostValue::to_strict`] / [`HostValue::from_strict`] bridge to [`pi_json::strict::Value`],
//! the durable payload type.

use std::any::Any;
use std::sync::Arc;

use pi_jsrt::js::{JsArray, JsUint8Array};
use pi_jsrt::js::{Context, JsObject, JsResult, JsString, JsValue};
use pi_json::strict::{self, JsonString};
use pi_jsrt::JsHandle;

use crate::errors::{ErrorClass, HostError, HostResult};

/// A value crossing the host boundary (see the module docs for the mapping).
#[derive(Clone, Debug, Default)]
pub enum HostValue {
    /// `undefined`, carried explicitly (an object member set to `undefined` stays a member).
    #[default]
    Undefined,
    /// `null`.
    Null,
    /// A boolean.
    Bool(bool),
    /// A number, bit-exact (`-0`, `NaN` and infinities included).
    Number(f64),
    /// A string as UTF-16 code units (lone surrogates included).
    String(JsonString),
    /// An array.
    Array(Vec<HostValue>),
    /// A plain object: own enumerable string keys in ECMAScript property order.
    Object(Vec<(JsonString, HostValue)>),
    /// A `Uint8Array`'s bytes.
    Bytes(Vec<u8>),
    /// A JS value kept by reference (function, class instance, symbol...).
    Handle(JsHandle),
    /// Rust state owned by an opaque JS object (see [`NativeRef`]).
    Native(NativeRef),
}

/// Rust state shared with JS as an opaque object: the object keeps the state alive until it is
/// garbage collected, and passing it back to the host yields the same state.
#[derive(Clone)]
pub struct NativeRef(pub Arc<dyn Any + Send + Sync>);

impl NativeRef {
    /// Wrap `value`.
    pub fn new<T: Any + Send + Sync>(value: T) -> Self {
        NativeRef(Arc::new(value))
    }

    /// The state as `T`, when it is one.
    pub fn downcast<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.0.clone().downcast::<T>().ok()
    }
}

impl std::fmt::Debug for NativeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeRef(..)")
    }
}

/// Object data of a [`NativeRef`]'s JS object.
struct NativeData(NativeRef);

impl pi_jsrt::js::JsData for NativeData {}

/// Object data of `__piDurable.ref(value)`: forces `value` across as a handle.
pub(crate) struct RefData(pub(crate) JsValue);

impl pi_jsrt::js::JsData for RefData {
    fn trace(&self, tracer: &pi_jsrt::js::Tracer) {
        tracer.mark(&self.0);
    }
}

impl HostValue {
    /// A string value from Rust text.
    pub fn string(text: &str) -> Self {
        HostValue::String(JsonString::from(text))
    }

    /// An object value from `(key, value)` pairs.
    pub fn object<K: AsRef<str>>(entries: impl IntoIterator<Item = (K, HostValue)>) -> Self {
        HostValue::Object(
            entries
                .into_iter()
                .map(|(k, v)| (JsonString::from(k.as_ref()), v))
                .collect(),
        )
    }

    /// Wrap Rust state for JS (see [`NativeRef`]).
    pub fn native<T: Any + Send + Sync>(value: T) -> Self {
        HostValue::Native(NativeRef::new(value))
    }

    /// Whether this is `undefined`.
    pub fn is_undefined(&self) -> bool {
        matches!(self, HostValue::Undefined)
    }

    /// The number, if this is one.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            HostValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// The boolean, if this is one.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            HostValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The string, if this is one.
    pub fn as_string(&self) -> Option<&JsonString> {
        match self {
            HostValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// The handle, if this is one.
    pub fn as_handle(&self) -> Option<&JsHandle> {
        match self {
            HostValue::Handle(h) => Some(h),
            _ => None,
        }
    }

    /// The bytes, if this is a `Uint8Array`.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            HostValue::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// Native state of type `T`, if this is a [`NativeRef`] holding one.
    pub fn as_native<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        match self {
            HostValue::Native(n) => n.downcast(),
            _ => None,
        }
    }

    /// Member `key` of an object (`None` when absent or not an object).
    pub fn get(&self, key: &str) -> Option<&HostValue> {
        match self {
            HostValue::Object(entries) => entries
                .iter()
                .find(|(k, _)| k.equals_str(key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Convert from strict JSON.
    pub fn from_strict(value: &strict::Value) -> Self {
        match value {
            strict::Value::Null => HostValue::Null,
            strict::Value::Bool(b) => HostValue::Bool(*b),
            strict::Value::Number(n) => HostValue::Number(n.as_f64().unwrap_or(f64::NAN)),
            strict::Value::String(s) => HostValue::String(s.clone()),
            strict::Value::Array(a) => {
                HostValue::Array(a.iter().map(HostValue::from_strict).collect())
            }
            strict::Value::Object(m) => HostValue::Object(
                m.iter()
                    .map(|(k, v)| (k.clone(), HostValue::from_strict(v)))
                    .collect(),
            ),
        }
    }

    /// Convert to strict JSON. `undefined` (anywhere), handles, native state, bytes and
    /// non-finite numbers are not JSON and fail with a `TypeError`.
    pub fn to_strict(&self) -> HostResult<strict::Value> {
        self.strict(false)
    }

    /// `JSON.stringify` semantics: `undefined` members are omitted and `undefined` array items
    /// become `null`; a top-level `undefined` and non-JSON values still fail.
    pub fn to_strict_omitting_undefined(&self) -> HostResult<strict::Value> {
        self.strict(true)
    }

    fn strict(&self, omit: bool) -> HostResult<strict::Value> {
        let not_json = |what: &str| {
            HostError::new(ErrorClass::TypeError, format!("{what} is not a JSON value"))
        };
        Ok(match self {
            HostValue::Undefined => return Err(not_json("undefined")),
            HostValue::Null => strict::Value::Null,
            HostValue::Bool(b) => strict::Value::Bool(*b),
            HostValue::Number(n) => {
                strict::Value::Number(number_to_json(*n).ok_or_else(|| not_json(&n.to_string()))?)
            }
            HostValue::String(s) => strict::Value::String(s.clone()),
            HostValue::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(match item {
                        HostValue::Undefined if omit => strict::Value::Null,
                        other => other.strict(omit)?,
                    });
                }
                strict::Value::Array(Arc::new(out))
            }
            HostValue::Object(entries) => {
                let mut map = strict::Map::with_capacity(entries.len());
                for (k, v) in entries {
                    if omit && v.is_undefined() {
                        continue;
                    }
                    map.insert(k.clone(), v.strict(omit)?);
                }
                strict::Value::Object(Arc::new(map))
            }
            HostValue::Bytes(_) => return Err(not_json("Uint8Array")),
            HostValue::Handle(_) => return Err(not_json("a JS object")),
            HostValue::Native(_) => return Err(not_json("a host object")),
        })
    }
}

impl From<strict::Value> for HostValue {
    fn from(value: strict::Value) -> Self {
        HostValue::from_strict(&value)
    }
}

impl From<&str> for HostValue {
    fn from(value: &str) -> Self {
        HostValue::string(value)
    }
}

impl From<JsonString> for HostValue {
    fn from(value: JsonString) -> Self {
        HostValue::String(value)
    }
}

impl From<f64> for HostValue {
    fn from(value: f64) -> Self {
        HostValue::Number(value)
    }
}

impl From<usize> for HostValue {
    fn from(value: usize) -> Self {
        HostValue::Number(value as f64)
    }
}

impl From<bool> for HostValue {
    fn from(value: bool) -> Self {
        HostValue::Bool(value)
    }
}

impl From<Vec<HostValue>> for HostValue {
    fn from(value: Vec<HostValue>) -> Self {
        HostValue::Array(value)
    }
}

impl<T: Into<HostValue>> From<Option<T>> for HostValue {
    fn from(value: Option<T>) -> Self {
        value.map_or(HostValue::Undefined, Into::into)
    }
}

fn number_to_json(n: f64) -> Option<serde_json::Number> {
    if !n.is_finite() {
        return None;
    }
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        return Some(serde_json::Number::from(n as i64));
    }
    serde_json::Number::from_f64(n)
}

/// A JS string's UTF-16 code units.
pub fn js_string_units(s: &JsString) -> JsonString {
    JsonString::from_utf16(s.to_utf16())
}

/// A JS string from UTF-16 code units.
pub fn units_js_string(s: &JsonString) -> JsString {
    JsString::from(s.units())
}

fn is_plain(obj: &JsObject, ctx: &mut Context) -> bool {
    match obj.prototype() {
        None => true,
        Some(proto) => JsObject::equals(
            &proto,
            &ctx.intrinsics().constructors().object().prototype(),
        ),
    }
}

fn type_error(message: impl Into<String>) -> HostError {
    HostError::new(ErrorClass::TypeError, message.into())
}

/// Convert a JS value to a [`HostValue`] (see the module docs).
pub fn from_js(ctx: &mut Context, value: &JsValue) -> HostResult<HostValue> {
    let mut stack = Vec::new();
    convert_in(ctx, value, &mut stack)
}

fn handle(ctx: &mut Context, value: JsValue) -> HostResult<HostValue> {
    pi_jsrt::ctx::make_handle(ctx, value)
        .map(HostValue::Handle)
        .ok_or_else(|| HostError::new(ErrorClass::Error, "the runtime is shutting down"))
}

fn convert_in(
    ctx: &mut Context,
    value: &JsValue,
    stack: &mut Vec<JsObject>,
) -> HostResult<HostValue> {
    if value.is_undefined() {
        return Ok(HostValue::Undefined);
    }
    if value.is_null() {
        return Ok(HostValue::Null);
    }
    if let Some(b) = value.as_boolean() {
        return Ok(HostValue::Bool(b));
    }
    if let Some(n) = value.as_number() {
        return Ok(HostValue::Number(n));
    }
    if let Some(s) = value.as_string() {
        return Ok(HostValue::String(js_string_units(&s)));
    }
    if value.is_bigint() {
        return Err(type_error(
            "a bigint cannot cross the pi-durable host boundary",
        ));
    }
    let Some(obj) = value.as_object() else {
        // Symbols.
        return handle(ctx, value.clone());
    };
    if let Some(data) = obj.downcast_ref::<RefData>() {
        let inner = data.0.clone();
        drop(data);
        return handle(ctx, inner);
    }
    if let Some(data) = obj.downcast_ref::<NativeData>() {
        return Ok(HostValue::Native(data.0.clone()));
    }
    if obj.is_callable() {
        return handle(ctx, value.clone());
    }
    if let Ok(bytes) = JsUint8Array::from_object(obj.clone()) {
        return bytes
            .to_vec(ctx)
            .map(HostValue::Bytes)
            .map_err(|e| HostError::from_error(ctx, e));
    }
    let array = obj.is_array();
    if !array && !is_plain(&obj, ctx) {
        return handle(ctx, value.clone());
    }
    if stack.iter().any(|o| JsObject::equals(o, &obj)) {
        return Err(type_error(
            "a cyclic structure cannot cross the pi-durable host boundary",
        ));
    }
    stack.push(obj.clone());
    let result = if array {
        convert_array(ctx, &obj, stack)
    } else {
        convert_object(ctx, &obj, stack)
    };
    stack.pop();
    result
}

fn convert_array(
    ctx: &mut Context,
    obj: &JsObject,
    stack: &mut Vec<JsObject>,
) -> HostResult<HostValue> {
    let len = obj
        .get(pi_jsrt::js::js_string!("length"), ctx)
        .and_then(|v| v.to_length(ctx))
        .map_err(|e| HostError::from_error(ctx, e))?;
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        let item = obj.get(i, ctx).map_err(|e| HostError::from_error(ctx, e))?;
        out.push(convert_in(ctx, &item, stack)?);
    }
    Ok(HostValue::Array(out))
}

fn convert_object(
    ctx: &mut Context,
    obj: &JsObject,
    stack: &mut Vec<JsObject>,
) -> HostResult<HostValue> {
    // `Object.keys`: own enumerable string keys in ECMAScript order.
    let keys = ctx
        .intrinsics()
        .constructors()
        .object()
        .constructor()
        .get(pi_jsrt::js::js_string!("keys"), ctx)
        .and_then(|keys| {
            let keys = keys.as_callable().ok_or_else(|| {
                pi_jsrt::js::JsNativeError::typ().with_message("Object.keys is missing")
            })?;
            keys.call(&JsValue::undefined(), &[obj.clone().into()], ctx)
        })
        .map_err(|e| HostError::from_error(ctx, e))?;
    let keys = keys
        .as_object()
        .ok_or_else(|| type_error("Object.keys did not return an array"))?;
    let len = keys
        .get(pi_jsrt::js::js_string!("length"), ctx)
        .and_then(|v| v.to_length(ctx))
        .map_err(|e| HostError::from_error(ctx, e))?;
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        let key = keys.get(i, ctx).map_err(|e| HostError::from_error(ctx, e))?;
        let Some(key) = key.as_string() else { continue };
        let v = obj
            .get(key.clone(), ctx)
            .map_err(|e| HostError::from_error(ctx, e))?;
        out.push((js_string_units(&key), convert_in(ctx, &v, stack)?));
    }
    Ok(HostValue::Object(out))
}

/// Convert a [`HostValue`] to a JS value (see the module docs).
pub fn to_js(ctx: &mut Context, value: &HostValue) -> JsResult<JsValue> {
    Ok(match value {
        HostValue::Undefined => JsValue::undefined(),
        HostValue::Null => JsValue::null(),
        HostValue::Bool(b) => JsValue::from(*b),
        HostValue::Number(n) => JsValue::from(*n),
        HostValue::String(s) => JsValue::from(units_js_string(s)),
        HostValue::Array(items) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(to_js(ctx, item)?);
            }
            JsArray::from_iter(values, ctx).into()
        }
        HostValue::Object(entries) => {
            let obj = JsObject::with_object_proto(ctx.intrinsics());
            for (k, v) in entries {
                let v = to_js(ctx, v)?;
                obj.create_data_property_or_throw(units_js_string(k), v, ctx)?;
            }
            obj.into()
        }
        HostValue::Bytes(bytes) => JsUint8Array::from_iter(bytes.iter().copied(), ctx)?.into(),
        HostValue::Handle(h) => pi_jsrt::ctx::handle_value(ctx, h).ok_or_else(|| {
            pi_jsrt::js::JsNativeError::typ().with_message(format!("invalid JS handle {}", h.id()))
        })?,
        HostValue::Native(n) => JsObject::from_proto_and_data(None, NativeData(n.clone())).into(),
    })
}

/// Convert a JS value to strict JSON (via [`from_js`] and [`HostValue::to_strict`]).
pub fn strict_from_js(ctx: &mut Context, value: &JsValue) -> HostResult<strict::Value> {
    from_js(ctx, value)?.to_strict()
}

/// Convert strict JSON to a JS value.
pub fn strict_to_js(ctx: &mut Context, value: &strict::Value) -> JsResult<JsValue> {
    to_js(ctx, &HostValue::from_strict(value))
}
