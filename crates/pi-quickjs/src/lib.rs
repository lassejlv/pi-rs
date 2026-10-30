//! A small embedding API over QuickJS ([rquickjs]).
//!
//! Embedders (pi-jsrt, pi-codemode) own one QuickJS runtime per thread, so values here are
//! plain refcounted QuickJS values bound to that thread: [`JsValue`] (and the typed wrappers
//! [`JsObject`], [`JsFunction`], [`JsString`], ...) duplicate on `clone` and free on `drop`,
//! with no lifetime tying them to a borrow of the context. The thread's [`Engine`] is reachable
//! through a thread-local, which is how values free themselves and how strings and objects can be
//! created without a context at hand. Values must not outlive their engine; values dropped after
//! it are leaked instead of freed.
//!
//! Rust code reaches the engine through [`Context`] (a cheap handle to one QuickJS context, i.e.
//! a realm). Native functions are `fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>`
//! ([`NativeFunction`]); errors are [`JsError`]s, either a thrown JS value or a
//! [`JsNativeError`] materialized lazily.

#![warn(missing_docs)]

use std::any::{Any, TypeId};
use std::borrow::Cow;
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::ffi::{CString, c_int, c_void};
use std::fmt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::Rc;

pub use rquickjs;
use rquickjs::qjs;

/// Result of fallible JS operations.
pub type JsResult<T> = Result<T, JsError>;

/// Additions of the vendored QuickJS (`vendor/rquickjs-sys/PATCH.md` in the pi-rs repository).
pub mod ext {
    use std::ffi::c_void;

    use rquickjs::qjs::{JSContext, JSRuntime, JSValue};

    /// Captures the host context of a promise job when it is created (a new reference).
    pub type JobContextCapture = unsafe extern "C" fn(ctx: *mut JSContext, opaque: *mut c_void) -> JSValue;
    /// Enters a job's host context; returns the previous one (a new reference).
    pub type JobContextEnter = unsafe extern "C" fn(ctx: *mut JSContext, job_context: JSValue, opaque: *mut c_void) -> JSValue;
    /// Leaves a job's host context, restoring (and taking) the previous one.
    pub type JobContextExit = unsafe extern "C" fn(ctx: *mut JSContext, previous: JSValue, opaque: *mut c_void);
    /// `(UTC - local time)` in minutes at `time_ms`.
    pub type TimezoneOffset = unsafe extern "C" fn(time_ms: i64, opaque: *mut c_void) -> i32;
    /// The current time of `Date`, in ms since the epoch.
    pub type DateNow = unsafe extern "C" fn(opaque: *mut c_void) -> i64;

    unsafe extern "C" {
        /// Install the promise job context hooks.
        pub fn JS_SetJobContextHooks(
            rt: *mut JSRuntime,
            capture: Option<JobContextCapture>,
            enter: Option<JobContextEnter>,
            exit: Option<JobContextExit>,
            opaque: *mut c_void,
        );
        /// Install the local timezone hook of `Date`.
        pub fn JS_SetTimezoneOffsetFunc(rt: *mut JSRuntime, func: Option<TimezoneOffset>, opaque: *mut c_void);
        /// Replace the global object of a context.
        pub fn JS_SetGlobalObject(ctx: *mut JSContext, obj: JSValue);
        /// Install the clock of `Date`.
        pub fn JS_SetDateNowFunc(rt: *mut JSRuntime, func: Option<DateNow>, opaque: *mut c_void);
    }
}

// ---------------------------------------------------------------------------
// Engine (one per thread)
// ---------------------------------------------------------------------------

struct EngineData {
    rt: *mut qjs::JSRuntime,
    main: Cell<*mut qjs::JSContext>,
    fn_class: qjs::JSClassID,
    data_class: qjs::JSClassID,
    realms: RefCell<Vec<*mut qjs::JSContext>>,
    intrinsics: RefCell<HashMap<usize, Rc<IntrinsicsCache>>>,
    data: RefCell<HashMap<TypeId, Box<dyn Any>>>,
}

thread_local! {
    static ENGINE: Cell<*const EngineData> = const { Cell::new(ptr::null()) };
}

fn try_engine() -> Option<&'static EngineData> {
    let p = ENGINE.with(|e| e.get());
    // SAFETY: the pointer is set while the owning `Engine` (which boxes the data) is alive on this
    // thread and cleared when it is dropped.
    if p.is_null() { None } else { Some(unsafe { &*p }) }
}

fn engine() -> &'static EngineData {
    try_engine().expect("no QuickJS engine on this thread")
}

/// The main context of this thread's engine.
fn main_ctx() -> *mut qjs::JSContext {
    engine().main.get()
}

/// Options for [`Engine::new`].
#[derive(Debug, Clone, Default)]
pub struct EngineOptions {
    /// Maximum JS stack size in bytes (`0` for QuickJS's default).
    pub max_stack_size: usize,
    /// Heap limit in bytes (`0` for none).
    pub memory_limit: usize,
}

/// A QuickJS runtime with its main context, bound to the current thread.
pub struct Engine {
    data: Box<EngineData>,
    context: Option<rquickjs::Context>,
    runtime: Option<rquickjs::Runtime>,
}

impl Engine {
    /// Create the engine for this thread (at most one engine per thread at a time).
    pub fn new(options: EngineOptions) -> Result<Engine, String> {
        if try_engine().is_some() {
            return Err("a QuickJS engine already exists on this thread".into());
        }
        let runtime = rquickjs::Runtime::new().map_err(|e| e.to_string())?;
        if options.memory_limit > 0 {
            runtime.set_memory_limit(options.memory_limit);
        }
        let context = rquickjs::Context::full(&runtime).map_err(|e| e.to_string())?;
        let main = context.as_raw().as_ptr();
        let rt = unsafe { qjs::JS_GetRuntime(main) };
        unsafe {
            // Measure the stack from here (rquickjs's setter treats limits above 16 MB as none).
            qjs::JS_UpdateStackTop(rt);
            if options.max_stack_size > 0 {
                qjs::JS_SetMaxStackSize(rt, options.max_stack_size as _);
            }
        }
        let fn_class = unsafe { register_class(rt, c"Function", Some(fn_finalizer), None, Some(fn_call)) };
        let data_class = unsafe { register_class(rt, c"HostObject", Some(data_finalizer), Some(data_mark), None) };
        let data = Box::new(EngineData {
            rt,
            main: Cell::new(main),
            fn_class,
            data_class,
            realms: RefCell::default(),
            intrinsics: RefCell::default(),
            data: RefCell::default(),
        });
        ENGINE.with(|e| e.set(&*data as *const EngineData));
        let engine = Engine { data, context: Some(context), runtime: Some(runtime) };
        register_intrinsics(main);
        Ok(engine)
    }

    /// The main context.
    pub fn context(&self) -> Context {
        Context::from_raw(self.data.main.get())
    }

    /// The underlying rquickjs runtime (limits, interrupt handler, hooks).
    pub fn runtime(&self) -> &rquickjs::Runtime {
        self.runtime.as_ref().expect("engine runtime")
    }

    /// Raw runtime pointer.
    pub fn raw_runtime(&self) -> *mut qjs::JSRuntime {
        self.data.rt
    }

    /// Drop everything stored with [`Context::insert_data`].
    pub fn clear_data(&self) {
        let data = std::mem::take(&mut *self.data.data.borrow_mut());
        drop(data);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.clear_data();
        self.data.intrinsics.borrow_mut().clear();
        unsafe {
            qjs::JS_RunGC(self.data.rt);
            for realm in std::mem::take(&mut *self.data.realms.borrow_mut()) {
                qjs::JS_FreeContext(realm);
            }
        }
        drop(self.context.take());
        drop(self.runtime.take());
        ENGINE.with(|e| e.set(ptr::null()));
    }
}

unsafe fn register_class(
    rt: *mut qjs::JSRuntime,
    name: &'static std::ffi::CStr,
    finalizer: qjs::JSClassFinalizer,
    gc_mark: qjs::JSClassGCMark,
    call: qjs::JSClassCall,
) -> qjs::JSClassID {
    let mut id: qjs::JSClassID = 0;
    unsafe {
        qjs::JS_NewClassID(rt, &mut id);
        let def = qjs::JSClassDef { class_name: name.as_ptr(), finalizer, gc_mark, call, exotic: ptr::null_mut() };
        qjs::JS_NewClass(rt, id, &def);
    }
    id
}

/// Run a full garbage collection.
pub fn run_gc() {
    if let Some(e) = try_engine() {
        unsafe { qjs::JS_RunGC(e.rt) }
    }
}

// ---------------------------------------------------------------------------
// Raw helpers
// ---------------------------------------------------------------------------

#[inline]
fn tag_of(v: qjs::JSValue) -> i32 {
    unsafe { qjs::JS_VALUE_GET_NORM_TAG(v) }
}

#[inline]
fn has_ref(v: qjs::JSValue) -> bool {
    unsafe { qjs::JS_VALUE_HAS_REF_COUNT(v) }
}

/// Take the pending exception of `ctx` as a [`JsError`].
pub fn take_exception(ctx: &Context) -> JsError {
    take_exception_raw(ctx.raw)
}

fn take_exception_raw(ctx: *mut qjs::JSContext) -> JsError {
    JsError::from_opaque(unsafe { JsValue::from_raw(qjs::JS_GetException(ctx)) })
}

/// Turn a raw value returned by a QuickJS call into a result (`JS_EXCEPTION` → the pending
/// exception), taking ownership of it.
pub fn check_value(ctx: &Context, v: qjs::JSValue) -> JsResult<JsValue> {
    check(ctx.raw, v)
}

fn check(ctx: *mut qjs::JSContext, v: qjs::JSValue) -> JsResult<JsValue> {
    if unsafe { qjs::JS_IsException(v) } {
        Err(take_exception_raw(ctx))
    } else {
        Ok(unsafe { JsValue::from_raw(v) })
    }
}

fn check_int(ctx: *mut qjs::JSContext, r: c_int) -> JsResult<c_int> {
    if r < 0 { Err(take_exception_raw(ctx)) } else { Ok(r) }
}

/// Throw `err` in `ctx` (for returning from QuickJS callbacks); returns `JS_EXCEPTION`.
pub fn throw_error(ctx: &Context, err: JsError) -> qjs::JSValue {
    throw(ctx.raw, err)
}

fn throw(ctx: *mut qjs::JSContext, err: JsError) -> qjs::JSValue {
    let mut c = Context::from_raw(ctx);
    let v = err.to_opaque(&mut c);
    unsafe { qjs::JS_Throw(ctx, v.into_raw()) }
}

fn panic_message(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "panic".to_string())
}

// ---------------------------------------------------------------------------
// JsValue
// ---------------------------------------------------------------------------

/// A JavaScript value (owned reference).
pub struct JsValue(qjs::JSValue);

struct StaticUndefined(JsValue);
// SAFETY: `undefined` holds no pointer and is never mutated or dropped.
unsafe impl Sync for StaticUndefined {}
static UNDEFINED: StaticUndefined = StaticUndefined(JsValue::undefined());

impl JsValue {
    /// `undefined`
    pub const fn undefined() -> Self {
        JsValue(qjs::JS_MKVAL(qjs::JS_TAG_UNDEFINED, 0))
    }

    /// `null`
    pub const fn null() -> Self {
        JsValue(qjs::JS_MKVAL(qjs::JS_TAG_NULL, 0))
    }

    /// Wrap a raw value, taking ownership of its reference.
    ///
    /// # Safety
    /// `v` must be a valid value of this thread's runtime whose reference the caller owns.
    pub unsafe fn from_raw(v: qjs::JSValue) -> Self {
        JsValue(v)
    }

    /// Wrap a borrowed raw value (duplicating it).
    ///
    /// # Safety
    /// `v` must be a valid value of this thread's runtime.
    pub unsafe fn from_raw_dup(v: qjs::JSValue) -> Self {
        if has_ref(v) {
            unsafe { qjs::JS_DupValueRT(engine().rt, v) };
        }
        JsValue(v)
    }

    /// The raw value (borrowed).
    pub fn as_raw(&self) -> qjs::JSValue {
        self.0
    }

    /// Give up ownership of the raw value.
    pub fn into_raw(self) -> qjs::JSValue {
        let v = self.0;
        std::mem::forget(self);
        v
    }

    fn tag(&self) -> i32 {
        tag_of(self.0)
    }

    /// Is `undefined`.
    pub fn is_undefined(&self) -> bool {
        self.tag() == qjs::JS_TAG_UNDEFINED
    }
    /// Is `null`.
    pub fn is_null(&self) -> bool {
        self.tag() == qjs::JS_TAG_NULL
    }
    /// Is `null` or `undefined`.
    pub fn is_null_or_undefined(&self) -> bool {
        self.is_null() || self.is_undefined()
    }
    /// Is a boolean.
    pub fn is_boolean(&self) -> bool {
        self.tag() == qjs::JS_TAG_BOOL
    }
    /// Is a number.
    pub fn is_number(&self) -> bool {
        matches!(self.tag(), qjs::JS_TAG_INT | qjs::JS_TAG_FLOAT64)
    }
    /// Is an integral number.
    pub fn is_integral_number(&self) -> bool {
        self.as_number().is_some_and(|n| n.is_finite() && n.fract() == 0.0)
    }
    /// Is a string.
    pub fn is_string(&self) -> bool {
        matches!(self.tag(), qjs::JS_TAG_STRING | qjs::JS_TAG_STRING_ROPE)
    }
    /// Is a symbol.
    pub fn is_symbol(&self) -> bool {
        self.tag() == qjs::JS_TAG_SYMBOL
    }
    /// Is a BigInt.
    pub fn is_bigint(&self) -> bool {
        matches!(self.tag(), qjs::JS_TAG_BIG_INT | qjs::JS_TAG_SHORT_BIG_INT)
    }
    /// Is an object (functions included).
    pub fn is_object(&self) -> bool {
        self.tag() == qjs::JS_TAG_OBJECT
    }
    /// Is callable.
    pub fn is_callable(&self) -> bool {
        self.is_object() && unsafe { qjs::JS_IsFunction(main_ctx(), self.0) }
    }
    /// Is a constructor.
    pub fn is_constructor(&self) -> bool {
        self.is_object() && unsafe { qjs::JS_IsConstructor(main_ctx(), self.0) }
    }
    /// Is a promise.
    pub fn is_promise(&self) -> bool {
        self.is_object() && unsafe { qjs::JS_IsPromise(self.0) }
    }

    /// The boolean, if this is one.
    pub fn as_boolean(&self) -> Option<bool> {
        (self.tag() == qjs::JS_TAG_BOOL).then(|| unsafe { qjs::JS_VALUE_GET_BOOL(self.0) })
    }
    /// The number, if this is one.
    pub fn as_number(&self) -> Option<f64> {
        match self.tag() {
            qjs::JS_TAG_INT => Some(unsafe { qjs::JS_VALUE_GET_INT(self.0) } as f64),
            qjs::JS_TAG_FLOAT64 => Some(unsafe { qjs::JS_VALUE_GET_FLOAT64(self.0) }),
            _ => None,
        }
    }
    /// The string, if this is one.
    pub fn as_string(&self) -> Option<JsString> {
        self.is_string().then(|| JsString(self.clone()))
    }
    /// The object, if this is one.
    pub fn as_object(&self) -> Option<JsObject> {
        self.is_object().then(|| JsObject(self.clone()))
    }
    /// The function, if this is callable.
    pub fn as_function(&self) -> Option<JsFunction> {
        self.is_callable().then(|| JsFunction(JsObject(self.clone())))
    }
    /// The callable object, if this is callable.
    pub fn as_callable(&self) -> Option<JsFunction> {
        self.as_function()
    }
    /// The constructor, if this is one.
    pub fn as_constructor(&self) -> Option<JsObject> {
        self.is_constructor().then(|| JsObject(self.clone()))
    }
    /// The promise, if this is one.
    pub fn as_promise(&self) -> Option<JsPromise> {
        self.is_promise().then(|| JsPromise(JsObject(self.clone())))
    }
    /// The symbol, if this is one.
    pub fn as_symbol(&self) -> Option<JsSymbol> {
        self.is_symbol().then(|| JsSymbol(self.clone()))
    }
    /// The BigInt, if this is one.
    pub fn as_bigint(&self) -> Option<JsBigInt> {
        self.is_bigint().then(|| JsBigInt(self.clone()))
    }

    /// `ToBoolean`.
    pub fn to_boolean(&self) -> bool {
        match self.tag() {
            qjs::JS_TAG_UNDEFINED | qjs::JS_TAG_NULL => false,
            qjs::JS_TAG_BOOL => unsafe { qjs::JS_VALUE_GET_BOOL(self.0) },
            qjs::JS_TAG_INT => (unsafe { qjs::JS_VALUE_GET_INT(self.0) }) != 0,
            qjs::JS_TAG_FLOAT64 => {
                let n = unsafe { qjs::JS_VALUE_GET_FLOAT64(self.0) };
                !(n == 0.0 || n.is_nan())
            }
            qjs::JS_TAG_OBJECT | qjs::JS_TAG_SYMBOL => true,
            _ => unsafe { qjs::JS_ToBool(main_ctx(), self.0) > 0 },
        }
    }

    /// `ToString`.
    pub fn to_string(&self, ctx: &mut Context) -> JsResult<JsString> {
        if self.is_string() {
            return Ok(JsString(self.clone()));
        }
        let v = check(ctx.raw, unsafe { qjs::JS_ToString(ctx.raw, self.0) })?;
        Ok(JsString(v))
    }

    /// `ToNumber`.
    pub fn to_number(&self, ctx: &mut Context) -> JsResult<f64> {
        if let Some(n) = self.as_number() {
            return Ok(n);
        }
        let mut out = 0f64;
        check_int(ctx.raw, unsafe { qjs::JS_ToFloat64(ctx.raw, &mut out, self.0) })?;
        Ok(out)
    }

    /// `ToInt32`.
    pub fn to_i32(&self, ctx: &mut Context) -> JsResult<i32> {
        let mut out = 0i32;
        check_int(ctx.raw, unsafe { qjs::JS_ToInt32(ctx.raw, &mut out, self.0) })?;
        Ok(out)
    }

    /// `ToUint32`.
    pub fn to_u32(&self, ctx: &mut Context) -> JsResult<u32> {
        Ok(self.to_i32(ctx)? as u32)
    }

    /// `ToIndex`.
    pub fn to_index(&self, ctx: &mut Context) -> JsResult<u64> {
        let mut out = 0u64;
        check_int(ctx.raw, unsafe { qjs::JS_ToIndex(ctx.raw, &mut out, self.0) })?;
        Ok(out)
    }

    /// `ToLength`.
    pub fn to_length(&self, ctx: &mut Context) -> JsResult<u64> {
        let n = self.to_number(ctx)?;
        if n.is_nan() || n <= 0.0 {
            return Ok(0);
        }
        Ok(n.min(9_007_199_254_740_991.0).trunc() as u64)
    }

    /// `ToIntegerOrInfinity` as an `f64` (truncated; `NaN` is `0`).
    pub fn to_integer_or_infinity(&self, ctx: &mut Context) -> JsResult<f64> {
        let n = self.to_number(ctx)?;
        Ok(if n.is_nan() { 0.0 } else { n.trunc() })
    }

    /// `ToObject`.
    pub fn to_object(&self, ctx: &mut Context) -> JsResult<JsObject> {
        if self.is_object() {
            return Ok(JsObject(self.clone()));
        }
        let v = check(ctx.raw, unsafe { qjs::JS_ToObject(ctx.raw, self.0) })?;
        Ok(JsObject(v))
    }

    /// `ToBigInt64` (BigInts and numbers).
    pub fn to_big_int64(&self, ctx: &mut Context) -> JsResult<i64> {
        let mut out = 0i64;
        check_int(ctx.raw, unsafe { qjs::JS_ToBigInt64(ctx.raw, &mut out, self.0) })?;
        Ok(out)
    }

    /// Whether this is an uncatchable error (interrupt or resource limit).
    pub fn is_uncatchable_error(&self) -> bool {
        self.is_object() && unsafe { qjs::JS_IsUncatchableError(self.0) }
    }

    /// `typeof value`.
    pub fn type_of(&self) -> &'static str {
        match self.tag() {
            qjs::JS_TAG_UNDEFINED => "undefined",
            qjs::JS_TAG_NULL => "object",
            qjs::JS_TAG_BOOL => "boolean",
            qjs::JS_TAG_INT | qjs::JS_TAG_FLOAT64 => "number",
            qjs::JS_TAG_STRING | qjs::JS_TAG_STRING_ROPE => "string",
            qjs::JS_TAG_SYMBOL => "symbol",
            qjs::JS_TAG_BIG_INT | qjs::JS_TAG_SHORT_BIG_INT => "bigint",
            qjs::JS_TAG_OBJECT if self.is_callable() => "function",
            _ => "object",
        }
    }

    /// `===`
    pub fn strict_equals(&self, other: &JsValue) -> bool {
        unsafe { qjs::JS_IsStrictEqual(main_ctx(), self.0, other.0) }
    }

    /// `SameValue`
    pub fn same_value(a: &JsValue, b: &JsValue) -> bool {
        unsafe { qjs::JS_IsSameValue(main_ctx(), a.0, b.0) }
    }

    /// A short description for diagnostics (never runs user code for primitives).
    pub fn display(&self) -> JsValueDisplay<'_> {
        JsValueDisplay(self)
    }

    /// Build a JS value from JSON.
    pub fn from_json(json: &serde_json::Value, ctx: &mut Context) -> JsResult<JsValue> {
        Ok(match json {
            serde_json::Value::Null => JsValue::null(),
            serde_json::Value::Bool(b) => JsValue::from(*b),
            serde_json::Value::Number(n) => JsValue::from(n.as_f64().unwrap_or(f64::NAN)),
            serde_json::Value::String(s) => JsValue::from(JsString::from(s.as_str())),
            serde_json::Value::Array(items) => {
                let arr = JsArray::new(ctx);
                for (i, item) in items.iter().enumerate() {
                    let v = JsValue::from_json(item, ctx)?;
                    arr.create_data_property_or_throw(i as u32, v, ctx)?;
                }
                arr.into()
            }
            serde_json::Value::Object(map) => {
                let obj = JsObject::with_object_proto(ctx.intrinsics());
                for (k, v) in map {
                    let v = JsValue::from_json(v, ctx)?;
                    obj.create_data_property_or_throw(k.as_str(), v, ctx)?;
                }
                obj.into()
            }
        })
    }

    /// `JSON.stringify` then parse into a `serde_json::Value` (`None` for `undefined`, functions
    /// and symbols).
    pub fn to_json(&self, ctx: &mut Context) -> JsResult<Option<serde_json::Value>> {
        let undefined = JsValue::undefined();
        let s = check(ctx.raw, unsafe { qjs::JS_JSONStringify(ctx.raw, self.0, undefined.0, undefined.0) })?;
        let Some(s) = s.as_string() else { return Ok(None) };
        serde_json::from_str(&s.to_std_string_lossy())
            .map(Some)
            .map_err(|e| JsNativeError::typ().with_message(format!("invalid JSON: {e}")).into())
    }
}

impl Default for JsValue {
    fn default() -> Self {
        JsValue::undefined()
    }
}

impl Clone for JsValue {
    fn clone(&self) -> Self {
        if has_ref(self.0) {
            match try_engine() {
                Some(e) => unsafe {
                    qjs::JS_DupValueRT(e.rt, self.0);
                },
                None => return JsValue::undefined(),
            }
        }
        JsValue(self.0)
    }
}

impl Drop for JsValue {
    fn drop(&mut self) {
        if has_ref(self.0)
            && let Some(e) = try_engine()
        {
            unsafe { qjs::JS_FreeValueRT(e.rt, self.0) }
        }
    }
}

impl PartialEq for JsValue {
    fn eq(&self, other: &Self) -> bool {
        JsValue::same_value(self, other)
    }
}

impl fmt::Debug for JsValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JsValue({})", self.display())
    }
}

/// See [`JsValue::display`].
pub struct JsValueDisplay<'a>(&'a JsValue);

impl fmt::Display for JsValueDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.0;
        if let Some(s) = v.as_string() {
            return write!(f, "{}", s.to_std_string_lossy());
        }
        if try_engine().is_none() {
            return write!(f, "<{}>", v.type_of());
        }
        let ctx = main_ctx();
        unsafe {
            let s = qjs::JS_ToString(ctx, v.0);
            if qjs::JS_IsException(s) {
                drop(JsValue::from_raw(qjs::JS_GetException(ctx)));
                return write!(f, "[{}]", v.type_of());
            }
            let s = JsString(JsValue::from_raw(s));
            write!(f, "{}", s.to_std_string_lossy())
        }
    }
}

fn number_value(n: f64) -> JsValue {
    if n.fract() == 0.0 && n >= i32::MIN as f64 && n <= i32::MAX as f64 && !(n == 0.0 && n.is_sign_negative()) {
        JsValue(qjs::JS_MKVAL(qjs::JS_TAG_INT, n as i32))
    } else {
        JsValue(qjs::JS_NewFloat64(n))
    }
}

macro_rules! from_number {
    ($($t:ty),*) => {$(
        impl From<$t> for JsValue {
            fn from(n: $t) -> Self {
                number_value(n as f64)
            }
        }
    )*};
}
from_number!(f64, f32, i8, i16, i32, i64, u8, u16, u32, u64, usize, isize);

impl From<bool> for JsValue {
    fn from(b: bool) -> Self {
        JsValue(qjs::JS_MKVAL(qjs::JS_TAG_BOOL, b as i32))
    }
}

impl From<()> for JsValue {
    fn from(_: ()) -> Self {
        JsValue::undefined()
    }
}

impl From<&str> for JsValue {
    fn from(s: &str) -> Self {
        JsString::from(s).into()
    }
}

impl From<String> for JsValue {
    fn from(s: String) -> Self {
        JsString::from(s.as_str()).into()
    }
}

impl<T: Into<JsValue>> From<Option<T>> for JsValue {
    fn from(v: Option<T>) -> Self {
        v.map_or_else(JsValue::undefined, Into::into)
    }
}

/// Argument access for native functions.
pub trait JsArgs {
    /// The argument at `index`, or `undefined`.
    fn get_or_undefined(&self, index: usize) -> &JsValue;
}

impl JsArgs for [JsValue] {
    fn get_or_undefined(&self, index: usize) -> &JsValue {
        self.get(index).unwrap_or(&UNDEFINED.0)
    }
}

// ---------------------------------------------------------------------------
// Strings, symbols, bigints
// ---------------------------------------------------------------------------

/// A JS string.
#[derive(Clone)]
pub struct JsString(JsValue);

/// Build a [`JsString`] from a `&str` expression.
#[macro_export]
macro_rules! js_string {
    () => {
        $crate::JsString::from("")
    };
    ($s:expr) => {
        $crate::JsString::from($s)
    };
}

impl JsString {
    /// Lossy conversion to a Rust string (lone surrogates become U+FFFD).
    pub fn to_std_string_lossy(&self) -> String {
        let ctx = match try_engine() {
            Some(e) => e.main.get(),
            None => return String::new(),
        };
        unsafe {
            let mut len: qjs::size_t = 0;
            let p = qjs::JS_ToCStringLen2(ctx, &mut len, self.0.0, false);
            let len = len as usize;
            if p.is_null() {
                drop(JsValue::from_raw(qjs::JS_GetException(ctx)));
                return String::new();
            }
            let bytes = std::slice::from_raw_parts(p as *const u8, len);
            let s = String::from_utf8_lossy(bytes).into_owned();
            qjs::JS_FreeCString(ctx, p);
            s
        }
    }

    /// Conversion to a Rust string.
    pub fn to_std_string(&self) -> Result<String, std::string::FromUtf8Error> {
        Ok(self.to_std_string_lossy())
    }

    /// Length in UTF-16 code units.
    pub fn len(&self) -> usize {
        self.to_utf16().len()
    }

    /// Is the empty string.
    pub fn is_empty(&self) -> bool {
        self.to_std_string_lossy().is_empty()
    }

    /// A string from UTF-16 code units (lone surrogates are kept).
    pub fn from_utf16(units: &[u16]) -> JsString {
        let ctx = main_ctx();
        let v = unsafe { qjs::JS_NewStringUTF16(ctx, units.as_ptr(), units.len() as _) };
        JsString(check(ctx, v).unwrap_or_default())
    }

    /// The UTF-16 code units (lone surrogates are kept).
    pub fn to_utf16(&self) -> Vec<u16> {
        let Some(e) = try_engine() else { return Vec::new() };
        let ctx = e.main.get();
        unsafe {
            let mut len: qjs::size_t = 0;
            // CESU-8: every UTF-16 unit outside the BMP-pair rules becomes its own sequence.
            let p = qjs::JS_ToCStringLen2(ctx, &mut len, self.0.0, true);
            if p.is_null() {
                drop(take_exception_raw(ctx));
                return Vec::new();
            }
            let bytes = std::slice::from_raw_parts(p as *const u8, len as usize);
            let units = decode_cesu8(bytes);
            qjs::JS_FreeCString(ctx, p);
            units
        }
    }

    /// The raw value.
    pub fn as_value(&self) -> &JsValue {
        &self.0
    }
}

/// Decode CESU-8 / WTF-8 bytes to UTF-16 code units.
fn decode_cesu8(bytes: &[u8]) -> Vec<u16> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let (cp, n) = if b < 0x80 {
            (b as u32, 1)
        } else if b >> 5 == 0b110 && i + 1 < bytes.len() {
            (((b as u32 & 0x1f) << 6) | (bytes[i + 1] as u32 & 0x3f), 2)
        } else if b >> 4 == 0b1110 && i + 2 < bytes.len() {
            (((b as u32 & 0x0f) << 12) | ((bytes[i + 1] as u32 & 0x3f) << 6) | (bytes[i + 2] as u32 & 0x3f), 3)
        } else if b >> 3 == 0b11110 && i + 3 < bytes.len() {
            let cp = ((b as u32 & 0x07) << 18) | ((bytes[i + 1] as u32 & 0x3f) << 12) | ((bytes[i + 2] as u32 & 0x3f) << 6) | (bytes[i + 3] as u32 & 0x3f);
            (cp, 4)
        } else {
            (0xfffd, 1)
        };
        if cp >= 0x10000 {
            let c = cp - 0x10000;
            out.push(0xd800 | (c >> 10) as u16);
            out.push(0xdc00 | (c & 0x3ff) as u16);
        } else {
            out.push(cp as u16);
        }
        i += n;
    }
    out
}

impl From<&[u16]> for JsString {
    fn from(units: &[u16]) -> Self {
        JsString::from_utf16(units)
    }
}

impl From<&str> for JsString {
    fn from(s: &str) -> Self {
        let ctx = main_ctx();
        let v = unsafe { qjs::JS_NewStringLen(ctx, s.as_ptr() as *const _, s.len() as _) };
        match check(ctx, v) {
            Ok(v) => JsString(v),
            Err(_) => JsString(JsValue::undefined()),
        }
    }
}

impl From<String> for JsString {
    fn from(s: String) -> Self {
        JsString::from(s.as_str())
    }
}

impl From<&String> for JsString {
    fn from(s: &String) -> Self {
        JsString::from(s.as_str())
    }
}

impl From<JsString> for JsValue {
    fn from(s: JsString) -> Self {
        s.0
    }
}

impl fmt::Display for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_std_string_lossy())
    }
}

impl fmt::Debug for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.to_std_string_lossy())
    }
}

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        self.0.strict_equals(&other.0)
    }
}
impl Eq for JsString {}

impl PartialEq<str> for JsString {
    fn eq(&self, other: &str) -> bool {
        self.to_std_string_lossy() == other
    }
}

impl PartialEq<&str> for JsString {
    fn eq(&self, other: &&str) -> bool {
        self.to_std_string_lossy() == *other
    }
}

impl std::hash::Hash for JsString {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.to_std_string_lossy().hash(state)
    }
}

/// A JS symbol.
#[derive(Clone, PartialEq)]
pub struct JsSymbol(JsValue);

impl JsSymbol {
    /// A new unique symbol.
    pub fn new(description: Option<JsString>) -> Option<JsSymbol> {
        let ctx = main_ctx();
        let desc = description.map(|d| d.to_std_string_lossy()).unwrap_or_default();
        let c = CString::new(desc.replace('\0', "")).ok()?;
        check(ctx, unsafe { qjs::JS_NewSymbol(ctx, c.as_ptr(), false) }).ok().map(JsSymbol)
    }

    /// `symbol.description`
    pub fn description(&self) -> Option<JsString> {
        let mut ctx = Context::from_raw(main_ctx());
        let key = PropertyKey::from("description");
        let atom = key.atom(&ctx).ok()?;
        let v = check(ctx.raw, unsafe { qjs::JS_GetProperty(ctx.raw, self.0.0, atom.0) }).ok()?;
        let _ = &mut ctx;
        v.as_string()
    }

    /// `Symbol(description)`
    pub fn descriptive_string(&self) -> JsString {
        let d = self.description().map(|d| d.to_std_string_lossy()).unwrap_or_default();
        JsString::from(format!("Symbol({d})").as_str())
    }
}

impl From<JsSymbol> for JsValue {
    fn from(s: JsSymbol) -> Self {
        s.0
    }
}

impl fmt::Debug for JsSymbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.descriptive_string())
    }
}

/// A JS BigInt.
#[derive(Clone, PartialEq)]
pub struct JsBigInt(JsValue);

impl JsBigInt {
    /// A BigInt from an `i64`.
    pub fn new(n: i64) -> JsBigInt {
        let ctx = main_ctx();
        JsBigInt(check(ctx, unsafe { qjs::JS_NewBigInt64(ctx, n) }).unwrap_or_default())
    }

    /// Parse a decimal BigInt literal.
    pub fn from_string(s: &str) -> Option<JsBigInt> {
        let mut ctx = Context::from_raw(main_ctx());
        let f = ctx.global_object().get("BigInt", &mut ctx).ok()?.as_function()?;
        let v = f.call(&JsValue::undefined(), &[JsValue::from(s)], &mut ctx).ok()?;
        v.as_bigint()
    }

    /// Lossy conversion to `f64`.
    pub fn to_f64(&self) -> f64 {
        self.to_string().parse().unwrap_or(f64::NAN)
    }

    /// Conversion to `i64` (wrapping, like `BigInt.asIntN(64, x)`).
    pub fn to_i64(&self) -> i64 {
        let ctx = main_ctx();
        let mut out = 0i64;
        unsafe { qjs::JS_ToBigInt64(ctx, &mut out, self.0.0) };
        out
    }
}

impl From<i64> for JsBigInt {
    fn from(n: i64) -> Self {
        JsBigInt::new(n)
    }
}

impl From<i32> for JsBigInt {
    fn from(n: i32) -> Self {
        JsBigInt::new(n as i64)
    }
}

impl From<u64> for JsBigInt {
    fn from(n: u64) -> Self {
        match i64::try_from(n) {
            Ok(n) => JsBigInt::new(n),
            Err(_) => JsBigInt::from_string(&n.to_string()).unwrap_or_else(|| JsBigInt::new(0)),
        }
    }
}

impl From<JsBigInt> for JsValue {
    fn from(b: JsBigInt) -> Self {
        b.0
    }
}

impl fmt::Display for JsBigInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ctx = main_ctx();
        match check(ctx, unsafe { qjs::JS_ToString(ctx, self.0.0) }) {
            Ok(s) => write!(f, "{}", JsString(s).to_std_string_lossy()),
            Err(_) => write!(f, "0"),
        }
    }
}

impl fmt::Debug for JsBigInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}n")
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The kind of a [`JsNativeError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsNativeErrorKind {
    /// `Error`
    Error,
    /// `TypeError`
    Type,
    /// `RangeError`
    Range,
    /// `SyntaxError`
    Syntax,
    /// `ReferenceError`
    Reference,
    /// `EvalError`
    Eval,
    /// `URIError`
    Uri,
    /// `InternalError`
    Internal,
}

impl JsNativeErrorKind {
    fn name(self) -> &'static str {
        match self {
            JsNativeErrorKind::Error => "Error",
            JsNativeErrorKind::Type => "TypeError",
            JsNativeErrorKind::Range => "RangeError",
            JsNativeErrorKind::Syntax => "SyntaxError",
            JsNativeErrorKind::Reference => "ReferenceError",
            JsNativeErrorKind::Eval => "EvalError",
            JsNativeErrorKind::Uri => "URIError",
            JsNativeErrorKind::Internal => "InternalError",
        }
    }
}

/// An error created by Rust code, materialized as a JS error object when thrown.
#[derive(Clone, Debug)]
pub struct JsNativeError {
    /// The error class.
    pub kind: JsNativeErrorKind,
    message: String,
    cause: Option<Box<JsError>>,
}

impl JsNativeError {
    fn new(kind: JsNativeErrorKind) -> Self {
        JsNativeError { kind, message: String::new(), cause: None }
    }
    /// `Error`
    pub fn error() -> Self {
        Self::new(JsNativeErrorKind::Error)
    }
    /// `TypeError`
    pub fn typ() -> Self {
        Self::new(JsNativeErrorKind::Type)
    }
    /// `RangeError`
    pub fn range() -> Self {
        Self::new(JsNativeErrorKind::Range)
    }
    /// `SyntaxError`
    pub fn syntax() -> Self {
        Self::new(JsNativeErrorKind::Syntax)
    }
    /// `ReferenceError`
    pub fn reference() -> Self {
        Self::new(JsNativeErrorKind::Reference)
    }
    /// `EvalError`
    pub fn eval() -> Self {
        Self::new(JsNativeErrorKind::Eval)
    }
    /// `URIError`
    pub fn uri() -> Self {
        Self::new(JsNativeErrorKind::Uri)
    }
    /// `InternalError`
    pub fn internal() -> Self {
        Self::new(JsNativeErrorKind::Internal)
    }
    /// Set the message.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }
    /// Set `error.cause`.
    pub fn with_cause(mut self, cause: impl Into<JsError>) -> Self {
        self.cause = Some(Box::new(cause.into()));
        self
    }
    /// The message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Create the JS error object.
    pub fn to_opaque(&self, ctx: &mut Context) -> JsObject {
        let raw = ctx.raw;
        let msg = CString::new(self.message.replace('\0', "\\0")).unwrap_or_default();
        let fmt = c"%s".as_ptr();
        let v = unsafe {
            match self.kind {
                JsNativeErrorKind::Type => qjs::JS_ThrowTypeError(raw, fmt, msg.as_ptr()),
                JsNativeErrorKind::Range => qjs::JS_ThrowRangeError(raw, fmt, msg.as_ptr()),
                JsNativeErrorKind::Syntax => qjs::JS_ThrowSyntaxError(raw, fmt, msg.as_ptr()),
                JsNativeErrorKind::Reference => qjs::JS_ThrowReferenceError(raw, fmt, msg.as_ptr()),
                JsNativeErrorKind::Internal => qjs::JS_ThrowInternalError(raw, fmt, msg.as_ptr()),
                JsNativeErrorKind::Error | JsNativeErrorKind::Eval | JsNativeErrorKind::Uri => {
                    qjs::JS_ThrowPlainError(raw, fmt, msg.as_ptr())
                }
            }
        };
        let _ = v;
        let obj = unsafe { JsValue::from_raw(qjs::JS_GetException(raw)) };
        let obj = JsObject(obj);
        if matches!(self.kind, JsNativeErrorKind::Eval | JsNativeErrorKind::Uri) {
            // Re-parent onto the right constructor's prototype.
            let global = ctx.global_object();
            if let Ok(ctor) = global.get(self.kind.name(), ctx)
                && let Some(ctor) = ctor.as_object()
                && let Ok(proto) = ctor.get("prototype", ctx)
                && let Some(proto) = proto.as_object()
            {
                obj.set_prototype(Some(proto));
            }
        }
        if let Some(cause) = &self.cause {
            let cause = cause.to_opaque(ctx);
            let _ = obj.define_property_or_throw(
                "cause",
                PropertyDescriptor::builder().value(cause).writable(true).enumerable(false).configurable(true),
                ctx,
            );
        }
        obj
    }
}

impl fmt::Display for JsNativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            write!(f, "{}", self.kind.name())
        } else {
            write!(f, "{}: {}", self.kind.name(), self.message)
        }
    }
}

#[derive(Clone)]
enum ErrorRepr {
    Native(JsNativeError),
    Opaque(JsValue),
}

/// A JS exception: a thrown value or a native error not yet materialized.
#[derive(Clone)]
pub struct JsError(ErrorRepr);

impl JsError {
    /// Wrap a thrown value.
    pub fn from_opaque(value: JsValue) -> Self {
        JsError(ErrorRepr::Opaque(value))
    }
    /// Wrap a native error.
    pub fn from_native(err: JsNativeError) -> Self {
        JsError(ErrorRepr::Native(err))
    }
    /// The thrown value, if this wraps one.
    pub fn as_opaque(&self) -> Option<&JsValue> {
        match &self.0 {
            ErrorRepr::Opaque(v) => Some(v),
            ErrorRepr::Native(_) => None,
        }
    }
    /// The native error, if this is one.
    pub fn as_native(&self) -> Option<&JsNativeError> {
        match &self.0 {
            ErrorRepr::Native(e) => Some(e),
            ErrorRepr::Opaque(_) => None,
        }
    }
    /// The thrown value (materializing a native error).
    pub fn to_opaque(&self, ctx: &mut Context) -> JsValue {
        match &self.0 {
            ErrorRepr::Opaque(v) => v.clone(),
            ErrorRepr::Native(e) => e.to_opaque(ctx).into(),
        }
    }
    /// The thrown value (materializing a native error).
    pub fn into_opaque(self, ctx: &mut Context) -> JsResult<JsValue> {
        Ok(match self.0 {
            ErrorRepr::Opaque(v) => v,
            ErrorRepr::Native(e) => e.to_opaque(ctx).into(),
        })
    }

    /// An error no `try` can catch (like QuickJS's interrupt): it ends the running script.
    pub fn uncatchable(message: &str, ctx: &mut Context) -> JsError {
        let obj = JsNativeError::internal().with_message(message).to_opaque(ctx);
        unsafe { qjs::JS_SetUncatchableError(ctx.raw, obj.as_raw()) };
        JsError::from_opaque(obj.into())
    }

    /// Whether this is an uncatchable error (interrupt or resource limit).
    pub fn is_uncatchable(&self) -> bool {
        match &self.0 {
            ErrorRepr::Opaque(v) => v.is_uncatchable_error(),
            ErrorRepr::Native(_) => false,
        }
    }
}

impl From<JsNativeError> for JsError {
    fn from(e: JsNativeError) -> Self {
        JsError::from_native(e)
    }
}

impl fmt::Display for JsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            ErrorRepr::Native(e) => write!(f, "{e}"),
            ErrorRepr::Opaque(v) => write!(f, "{}", v.display()),
        }
    }
}

impl fmt::Debug for JsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JsError({self})")
    }
}

impl std::error::Error for JsError {}

// ---------------------------------------------------------------------------
// Context, realms, intrinsics
// ---------------------------------------------------------------------------

/// A handle to one QuickJS context (a realm) of this thread's engine.
#[derive(Clone)]
pub struct Context {
    raw: *mut qjs::JSContext,
    realm: Realm,
    intrinsics: Intrinsics,
}

/// A realm (QuickJS context). Realms live until the engine is dropped.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Realm(*mut qjs::JSContext);

impl Realm {
    /// The raw context.
    pub fn as_raw(&self) -> *mut qjs::JSContext {
        self.0
    }
}

/// Source text for [`Context::eval`].
pub struct Source<'a> {
    code: Cow<'a, [u8]>,
    path: Option<PathBuf>,
}

impl<'a> Source<'a> {
    /// Source from bytes (UTF-8).
    pub fn from_bytes<B: AsRef<[u8]> + ?Sized>(code: &'a B) -> Self {
        Source { code: Cow::Borrowed(code.as_ref()), path: None }
    }
    /// Set the file name used in stack traces.
    pub fn with_path(mut self, path: &Path) -> Self {
        self.path = Some(path.to_path_buf());
        self
    }
}

impl Context {
    /// A handle for a raw context of this thread's engine.
    pub fn from_raw(raw: *mut qjs::JSContext) -> Context {
        Context { raw, realm: Realm(raw), intrinsics: Intrinsics { ctx: raw } }
    }

    /// The raw context.
    pub fn as_raw(&self) -> *mut qjs::JSContext {
        self.raw
    }

    /// The raw runtime.
    pub fn raw_runtime(&self) -> *mut qjs::JSRuntime {
        unsafe { qjs::JS_GetRuntime(self.raw) }
    }

    /// This context's realm.
    pub fn realm(&self) -> &Realm {
        &self.realm
    }

    /// A context for `realm`.
    pub fn for_realm(realm: &Realm) -> Context {
        Context::from_raw(realm.0)
    }

    /// Replace this realm's global object (`this` of scripts, where global names resolve).
    pub fn set_global_object(&mut self, global: &JsObject) {
        unsafe { ext::JS_SetGlobalObject(self.raw, global.as_raw()) }
    }

    /// Create a new realm (with its own intrinsics) in this engine.
    pub fn create_realm(&mut self) -> JsResult<Realm> {
        let e = engine();
        let raw = unsafe { qjs::JS_NewContext(e.rt) };
        if raw.is_null() {
            return Err(JsNativeError::error().with_message("failed to create realm").into());
        }
        e.realms.borrow_mut().push(raw);
        register_intrinsics(raw);
        Ok(Realm(raw))
    }

    /// The well-known constructors of this realm.
    pub fn intrinsics(&self) -> &Intrinsics {
        &self.intrinsics
    }

    /// `globalThis`
    pub fn global_object(&self) -> JsObject {
        JsObject(unsafe { JsValue::from_raw(qjs::JS_GetGlobalObject(self.raw)) })
    }

    /// Evaluate a classic script in the global scope.
    pub fn eval(&mut self, source: Source<'_>) -> JsResult<JsValue> {
        let name = source.path.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "<eval>".into());
        self.eval_with_flags(&source.code, &name, qjs::JS_EVAL_TYPE_GLOBAL as i32)
    }

    /// Evaluate source with raw `JS_EVAL_*` flags.
    pub fn eval_with_flags(&mut self, code: &[u8], filename: &str, flags: i32) -> JsResult<JsValue> {
        let mut buf = Vec::with_capacity(code.len() + 1);
        buf.extend_from_slice(code);
        buf.push(0);
        let name = CString::new(filename.replace('\0', "")).unwrap_or_default();
        let mut opts = qjs::JSEvalOptions { version: 1, eval_flags: flags, filename: name.as_ptr(), line_num: 1 };
        let v = unsafe { qjs::JS_Eval2(self.raw, buf.as_ptr() as *const _, code.len() as _, &mut opts) };
        check(self.raw, v)
    }

    /// Store per-engine data (one value per type).
    pub fn insert_data<T: 'static>(&mut self, value: T) {
        engine().data.borrow_mut().insert(TypeId::of::<T>(), Box::new(value));
    }

    /// Per-engine data stored with [`Context::insert_data`].
    pub fn get_data<T: 'static>(&self) -> Option<&T> {
        let e = try_engine()?;
        let map = e.data.borrow();
        let b = map.get(&TypeId::of::<T>())?;
        let p = b.downcast_ref::<T>()? as *const T;
        // SAFETY: entries are boxed (stable address) and only removed by `Engine::clear_data`
        // at teardown.
        Some(unsafe { &*p })
    }

    /// Run one pending promise job: `None` when the queue is empty, otherwise the job's outcome.
    pub fn run_pending_job(&mut self) -> Option<JsResult<()>> {
        let mut pctx: *mut qjs::JSContext = ptr::null_mut();
        let r = unsafe { qjs::JS_ExecutePendingJob(self.raw_runtime(), &mut pctx) };
        match r {
            0 => None,
            r if r > 0 => Some(Ok(())),
            _ => {
                let ctx = if pctx.is_null() { self.raw } else { pctx };
                Some(Err(take_exception_raw(ctx)))
            }
        }
    }

    /// Whether promise jobs are queued.
    pub fn has_pending_jobs(&self) -> bool {
        unsafe { qjs::JS_IsJobPending(self.raw_runtime()) }
    }

    /// Kept for API compatibility (QuickJS has no kept-objects list).
    pub fn clear_kept_objects(&mut self) {}
}

struct IntrinsicsCache {
    object: (JsObject, JsObject),
    symbol: (JsObject, JsObject),
    function: (JsObject, JsObject),
    array: (JsObject, JsObject),
    promise: (JsObject, JsObject),
}

fn register_intrinsics(raw: *mut qjs::JSContext) {
    let mut ctx = Context::from_raw(raw);
    let global = ctx.global_object();
    let mut pair = |name: &str| -> (JsObject, JsObject) {
        let ctor = global.get(name, &mut ctx).ok().and_then(|v| v.as_object()).unwrap_or_else(JsObject::with_null_proto);
        let proto = ctor.get("prototype", &mut ctx).ok().and_then(|v| v.as_object()).unwrap_or_else(JsObject::with_null_proto);
        (ctor, proto)
    };
    let cache = IntrinsicsCache {
        object: pair("Object"),
        symbol: pair("Symbol"),
        function: pair("Function"),
        array: pair("Array"),
        promise: pair("Promise"),
    };
    engine().intrinsics.borrow_mut().insert(raw as usize, Rc::new(cache));
}

/// The well-known intrinsic objects of a realm.
#[derive(Clone)]
pub struct Intrinsics {
    ctx: *mut qjs::JSContext,
}

impl Intrinsics {
    fn cache(&self) -> Rc<IntrinsicsCache> {
        engine().intrinsics.borrow().get(&(self.ctx as usize)).cloned().expect("realm intrinsics")
    }
    /// The standard constructors.
    pub fn constructors(&self) -> Constructors {
        Constructors(self.cache())
    }
    /// The raw context.
    pub fn as_raw(&self) -> *mut qjs::JSContext {
        self.ctx
    }
}

/// See [`Intrinsics::constructors`].
pub struct Constructors(Rc<IntrinsicsCache>);

/// A constructor and its prototype.
pub struct StandardConstructor(JsObject, JsObject);

impl StandardConstructor {
    /// The constructor.
    pub fn constructor(&self) -> JsObject {
        self.0.clone()
    }
    /// `constructor.prototype`
    pub fn prototype(&self) -> JsObject {
        self.1.clone()
    }
}

impl Constructors {
    /// `Object`
    pub fn object(&self) -> StandardConstructor {
        StandardConstructor(self.0.object.0.clone(), self.0.object.1.clone())
    }
    /// `Symbol`
    pub fn symbol(&self) -> StandardConstructor {
        StandardConstructor(self.0.symbol.0.clone(), self.0.symbol.1.clone())
    }
    /// `Function`
    pub fn function(&self) -> StandardConstructor {
        StandardConstructor(self.0.function.0.clone(), self.0.function.1.clone())
    }
    /// `Array`
    pub fn array(&self) -> StandardConstructor {
        StandardConstructor(self.0.array.0.clone(), self.0.array.1.clone())
    }
    /// `Promise`
    pub fn promise(&self) -> StandardConstructor {
        StandardConstructor(self.0.promise.0.clone(), self.0.promise.1.clone())
    }
}

// ---------------------------------------------------------------------------
// Property keys and descriptors
// ---------------------------------------------------------------------------

/// A property key.
#[derive(Clone, Debug)]
pub enum PropertyKey {
    /// A string key from Rust text (no JS string is created to look it up).
    Name(Rc<str>),
    /// A string key.
    String(JsString),
    /// A symbol key.
    Symbol(JsSymbol),
    /// An array index.
    Index(u32),
}

struct Atom(qjs::JSAtom, *mut qjs::JSContext);

impl Drop for Atom {
    fn drop(&mut self) {
        unsafe { qjs::JS_FreeAtom(self.1, self.0) }
    }
}

impl PropertyKey {
    fn atom(&self, ctx: &Context) -> JsResult<Atom> {
        let a = unsafe {
            match self {
                PropertyKey::Index(i) => qjs::JS_NewAtomUInt32(ctx.raw, *i),
                PropertyKey::Name(s) => qjs::JS_NewAtomLen(ctx.raw, s.as_ptr() as *const _, s.len() as _),
                PropertyKey::String(s) => qjs::JS_ValueToAtom(ctx.raw, s.0.0),
                PropertyKey::Symbol(s) => qjs::JS_ValueToAtom(ctx.raw, s.0.0),
            }
        };
        if a == qjs::JS_ATOM_NULL {
            return Err(take_exception_raw(ctx.raw));
        }
        Ok(Atom(a, ctx.raw))
    }

    fn from_atom(ctx: *mut qjs::JSContext, atom: qjs::JSAtom) -> PropertyKey {
        let v = unsafe { JsValue::from_raw(qjs::JS_AtomToValue(ctx, atom)) };
        if v.is_symbol() {
            return PropertyKey::Symbol(JsSymbol(v));
        }
        if let Some(n) = v.as_number() {
            return PropertyKey::Index(n as u32);
        }
        let s = JsString(v);
        if let Ok(i) = s.to_std_string_lossy().parse::<u32>()
            && i != u32::MAX
            && i.to_string() == s.to_std_string_lossy()
        {
            return PropertyKey::Index(i);
        }
        PropertyKey::String(s)
    }

    /// The key as a JS value (indices become strings).
    pub fn to_value(&self) -> JsValue {
        match self {
            PropertyKey::Name(s) => JsString::from(&**s).into(),
            PropertyKey::String(s) => s.clone().into(),
            PropertyKey::Symbol(s) => s.clone().into(),
            PropertyKey::Index(i) => JsString::from(i.to_string().as_str()).into(),
        }
    }
}

impl fmt::Display for PropertyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PropertyKey::Name(s) => write!(f, "{s}"),
            PropertyKey::String(s) => write!(f, "{s}"),
            PropertyKey::Symbol(s) => write!(f, "{}", s.descriptive_string()),
            PropertyKey::Index(i) => write!(f, "{i}"),
        }
    }
}

impl From<&str> for PropertyKey {
    fn from(s: &str) -> Self {
        PropertyKey::Name(Rc::from(s))
    }
}
impl From<String> for PropertyKey {
    fn from(s: String) -> Self {
        PropertyKey::Name(Rc::from(s))
    }
}
impl From<&String> for PropertyKey {
    fn from(s: &String) -> Self {
        PropertyKey::Name(Rc::from(s.as_str()))
    }
}
impl From<JsString> for PropertyKey {
    fn from(s: JsString) -> Self {
        PropertyKey::String(s)
    }
}
impl From<&JsString> for PropertyKey {
    fn from(s: &JsString) -> Self {
        PropertyKey::String(s.clone())
    }
}
impl From<JsSymbol> for PropertyKey {
    fn from(s: JsSymbol) -> Self {
        PropertyKey::Symbol(s)
    }
}
impl From<u32> for PropertyKey {
    fn from(i: u32) -> Self {
        PropertyKey::Index(i)
    }
}
impl From<usize> for PropertyKey {
    fn from(i: usize) -> Self {
        match u32::try_from(i) {
            Ok(i) if i != u32::MAX => PropertyKey::Index(i),
            _ => PropertyKey::from(i.to_string()),
        }
    }
}
impl From<u64> for PropertyKey {
    fn from(i: u64) -> Self {
        PropertyKey::from(i as usize)
    }
}
impl From<i32> for PropertyKey {
    fn from(i: i32) -> Self {
        if i >= 0 { PropertyKey::Index(i as u32) } else { PropertyKey::from(i.to_string()) }
    }
}
impl From<i64> for PropertyKey {
    fn from(i: i64) -> Self {
        if i >= 0 { PropertyKey::from(i as u64) } else { PropertyKey::from(i.to_string()) }
    }
}

/// A property descriptor for [`JsObject::define_property_or_throw`].
#[derive(Clone, Default)]
pub struct PropertyDescriptor {
    value: Option<JsValue>,
    get: Option<JsValue>,
    set: Option<JsValue>,
    writable: Option<bool>,
    enumerable: Option<bool>,
    configurable: Option<bool>,
}

/// Builder for [`PropertyDescriptor`].
#[derive(Clone, Default)]
pub struct PropertyDescriptorBuilder(PropertyDescriptor);

impl PropertyDescriptor {
    /// Start a descriptor.
    pub fn builder() -> PropertyDescriptorBuilder {
        PropertyDescriptorBuilder::default()
    }
    /// The value, if this is a data descriptor with one.
    pub fn value(&self) -> Option<&JsValue> {
        self.value.as_ref()
    }
}

impl PropertyDescriptorBuilder {
    /// `value`
    pub fn value(mut self, v: impl Into<JsValue>) -> Self {
        self.0.value = Some(v.into());
        self
    }
    /// `get`
    pub fn get(mut self, f: impl Into<JsValue>) -> Self {
        self.0.get = Some(f.into());
        self
    }
    /// `set`
    pub fn set(mut self, f: impl Into<JsValue>) -> Self {
        self.0.set = Some(f.into());
        self
    }
    /// `writable`
    pub fn writable(mut self, b: bool) -> Self {
        self.0.writable = Some(b);
        self
    }
    /// `enumerable`
    pub fn enumerable(mut self, b: bool) -> Self {
        self.0.enumerable = Some(b);
        self
    }
    /// `configurable`
    pub fn configurable(mut self, b: bool) -> Self {
        self.0.configurable = Some(b);
        self
    }
    /// Finish.
    pub fn build(self) -> PropertyDescriptor {
        self.0
    }
}

impl From<PropertyDescriptorBuilder> for PropertyDescriptor {
    fn from(b: PropertyDescriptorBuilder) -> Self {
        b.0
    }
}

// ---------------------------------------------------------------------------
// Objects
// ---------------------------------------------------------------------------

/// A JS object (functions, arrays, ... included).
#[derive(Clone)]
pub struct JsObject(JsValue);

/// Rust data attached to an object made by [`JsObject::from_proto_and_data`].
pub trait JsData: Any {
    /// Report JS values held by this data to the cycle collector (optional: unreported values
    /// stay alive, which can leak cycles but never frees too early).
    fn trace(&self, _tracer: &Tracer) {}
}

/// Marks values for QuickJS's cycle collector (see [`JsData::trace`]).
pub struct Tracer {
    rt: *mut qjs::JSRuntime,
    mark: qjs::JS_MarkFunc,
}

impl Tracer {
    /// Report `value` as referenced.
    pub fn mark(&self, value: &JsValue) {
        unsafe { qjs::JS_MarkValue(self.rt, value.0, self.mark) }
    }
}

trait DataDyn: Any {
    fn trace_dyn(&self, tracer: &Tracer);
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: JsData> DataDyn for T {
    fn trace_dyn(&self, tracer: &Tracer) {
        self.trace(tracer)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

struct DataBox(RefCell<Box<dyn DataDyn>>);

unsafe extern "C" fn data_finalizer(_rt: *mut qjs::JSRuntime, val: qjs::JSValue) {
    let Some(e) = try_engine() else { return };
    let p = unsafe { qjs::JS_GetOpaque(val, e.data_class) } as *mut DataBox;
    if !p.is_null() {
        let b = unsafe { Box::from_raw(p) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(b)));
    }
}

unsafe extern "C" fn data_mark(rt: *mut qjs::JSRuntime, val: qjs::JSValue, mark: qjs::JS_MarkFunc) {
    let Some(e) = try_engine() else { return };
    let p = unsafe { qjs::JS_GetOpaque(val, e.data_class) } as *const DataBox;
    if p.is_null() {
        return;
    }
    if let Ok(data) = unsafe { &*p }.0.try_borrow() {
        data.trace_dyn(&Tracer { rt, mark });
    }
}

impl JsObject {
    /// An ordinary object with `Object.prototype` of the realm of `intrinsics`.
    pub fn with_object_proto(intrinsics: &Intrinsics) -> JsObject {
        let ctx = intrinsics.ctx;
        JsObject(check(ctx, unsafe { qjs::JS_NewObject(ctx) }).expect("out of memory"))
    }

    /// An object with a `null` prototype.
    pub fn with_null_proto() -> JsObject {
        let ctx = main_ctx();
        JsObject(check(ctx, unsafe { qjs::JS_NewObjectProto(ctx, JsValue::null().0) }).expect("out of memory"))
    }

    /// An ordinary object with the given prototype.
    pub fn with_proto(proto: Option<&JsObject>) -> JsObject {
        let ctx = main_ctx();
        let p = proto.map_or(JsValue::null().0, |p| p.0.0);
        JsObject(check(ctx, unsafe { qjs::JS_NewObjectProto(ctx, p) }).expect("out of memory"))
    }

    /// An object owning Rust `data` (see [`JsObject::downcast_ref`]).
    pub fn from_proto_and_data<T: JsData>(proto: Option<JsObject>, data: T) -> JsObject {
        let e = engine();
        let ctx = e.main.get();
        let p = proto.as_ref().map_or(JsValue::null().0, |p| p.0.0);
        let v = check(ctx, unsafe { qjs::JS_NewObjectProtoClass(ctx, p, e.data_class) }).expect("out of memory");
        let b: Box<DataBox> = Box::new(DataBox(RefCell::new(Box::new(data))));
        unsafe { qjs::JS_SetOpaque(v.0, Box::into_raw(b) as *mut c_void) };
        JsObject(v)
    }

    fn data_box(&self) -> Option<&DataBox> {
        let e = try_engine()?;
        let p = unsafe { qjs::JS_GetOpaque(self.0.0, e.data_class) } as *const DataBox;
        // SAFETY: the box lives as long as the object, which `self` keeps alive.
        (!p.is_null()).then(|| unsafe { &*p })
    }

    /// Borrow the Rust data of type `T`.
    pub fn downcast_ref<T: 'static>(&self) -> Option<Ref<'_, T>> {
        let b = self.data_box()?;
        let r = b.0.try_borrow().ok()?;
        Ref::filter_map(r, |d| d.as_any().downcast_ref::<T>()).ok()
    }

    /// Mutably borrow the Rust data of type `T`.
    pub fn downcast_mut<T: 'static>(&self) -> Option<RefMut<'_, T>> {
        let b = self.data_box()?;
        let r = b.0.try_borrow_mut().ok()?;
        RefMut::filter_map(r, |d| d.as_any_mut().downcast_mut::<T>()).ok()
    }

    /// Whether the object owns Rust data of type `T`.
    pub fn is<T: 'static>(&self) -> bool {
        self.data_box().is_some_and(|b| b.0.try_borrow().map(|d| d.as_any().is::<T>()).unwrap_or(false))
    }

    /// The raw value.
    pub fn as_raw(&self) -> qjs::JSValue {
        self.0.0
    }

    /// As a value.
    pub fn as_value(&self) -> &JsValue {
        &self.0
    }

    /// `obj[key]`
    pub fn get<K: Into<PropertyKey>>(&self, key: K, ctx: &mut Context) -> JsResult<JsValue> {
        let atom = key.into().atom(ctx)?;
        check(ctx.raw, unsafe { qjs::JS_GetProperty(ctx.raw, self.0.0, atom.0) })
    }

    /// `obj[key] = value` (throws on failure, like strict mode code).
    pub fn set<K: Into<PropertyKey>, V: Into<JsValue>>(&self, key: K, value: V, _throw: bool, ctx: &mut Context) -> JsResult<bool> {
        let atom = key.into().atom(ctx)?;
        let v = value.into().into_raw();
        let r = unsafe { qjs::JS_SetProperty(ctx.raw, self.0.0, atom.0, v) };
        Ok(check_int(ctx.raw, r)? != 0)
    }

    /// `CreateDataPropertyOrThrow`
    pub fn create_data_property_or_throw<K: Into<PropertyKey>, V: Into<JsValue>>(&self, key: K, value: V, ctx: &mut Context) -> JsResult<bool> {
        let atom = key.into().atom(ctx)?;
        let v = value.into().into_raw();
        let r = unsafe { qjs::JS_DefinePropertyValue(ctx.raw, self.0.0, atom.0, v, (qjs::JS_PROP_C_W_E | qjs::JS_PROP_THROW) as i32) };
        Ok(check_int(ctx.raw, r)? != 0)
    }

    /// `CreateDataProperty` (no throw on failure).
    pub fn create_data_property<K: Into<PropertyKey>, V: Into<JsValue>>(&self, key: K, value: V, ctx: &mut Context) -> JsResult<bool> {
        let atom = key.into().atom(ctx)?;
        let v = value.into().into_raw();
        let r = unsafe { qjs::JS_DefinePropertyValue(ctx.raw, self.0.0, atom.0, v, qjs::JS_PROP_C_W_E as i32) };
        Ok(check_int(ctx.raw, r)? != 0)
    }

    /// `Object.defineProperty`
    pub fn define_property_or_throw<K: Into<PropertyKey>, P: Into<PropertyDescriptor>>(&self, key: K, desc: P, ctx: &mut Context) -> JsResult<bool> {
        let atom = key.into().atom(ctx)?;
        let d: PropertyDescriptor = desc.into();
        let mut flags = qjs::JS_PROP_THROW as i32;
        let undefined = JsValue::undefined();
        let mut set_flag = |has: u32, bit: u32, v: Option<bool>| {
            if let Some(b) = v {
                flags |= has as i32;
                if b {
                    flags |= bit as i32;
                }
            }
        };
        set_flag(qjs::JS_PROP_HAS_WRITABLE, qjs::JS_PROP_WRITABLE, d.writable);
        set_flag(qjs::JS_PROP_HAS_ENUMERABLE, qjs::JS_PROP_ENUMERABLE, d.enumerable);
        set_flag(qjs::JS_PROP_HAS_CONFIGURABLE, qjs::JS_PROP_CONFIGURABLE, d.configurable);
        if d.value.is_some() {
            flags |= qjs::JS_PROP_HAS_VALUE as i32;
        }
        if d.get.is_some() {
            flags |= qjs::JS_PROP_HAS_GET as i32;
        }
        if d.set.is_some() {
            flags |= qjs::JS_PROP_HAS_SET as i32;
        }
        let value = d.value.as_ref().unwrap_or(&undefined);
        let get = d.get.as_ref().unwrap_or(&undefined);
        let set = d.set.as_ref().unwrap_or(&undefined);
        let r = unsafe { qjs::JS_DefineProperty(ctx.raw, self.0.0, atom.0, value.0, get.0, set.0, flags) };
        Ok(check_int(ctx.raw, r)? != 0)
    }

    /// `key in obj`
    pub fn has_property<K: Into<PropertyKey>>(&self, key: K, ctx: &mut Context) -> JsResult<bool> {
        let atom = key.into().atom(ctx)?;
        Ok(check_int(ctx.raw, unsafe { qjs::JS_HasProperty(ctx.raw, self.0.0, atom.0) })? != 0)
    }

    /// `Object.hasOwn(obj, key)`
    pub fn has_own_property<K: Into<PropertyKey>>(&self, key: K, ctx: &mut Context) -> JsResult<bool> {
        let atom = key.into().atom(ctx)?;
        Ok(check_int(ctx.raw, unsafe { qjs::JS_GetOwnProperty(ctx.raw, ptr::null_mut(), self.0.0, atom.0) })? != 0)
    }

    /// Own property descriptor (`None` if absent).
    pub fn get_own_property<K: Into<PropertyKey>>(&self, key: K, ctx: &mut Context) -> JsResult<Option<PropertyDescriptor>> {
        let atom = key.into().atom(ctx)?;
        let mut desc = qjs::JSPropertyDescriptor {
            flags: 0,
            value: JsValue::undefined().0,
            getter: JsValue::undefined().0,
            setter: JsValue::undefined().0,
        };
        let r = check_int(ctx.raw, unsafe { qjs::JS_GetOwnProperty(ctx.raw, &mut desc, self.0.0, atom.0) })?;
        if r == 0 {
            return Ok(None);
        }
        let (value, getter, setter) = unsafe { (JsValue::from_raw(desc.value), JsValue::from_raw(desc.getter), JsValue::from_raw(desc.setter)) };
        let f = desc.flags as u32;
        let accessor = f & qjs::JS_PROP_GETSET != 0;
        Ok(Some(PropertyDescriptor {
            value: (!accessor).then_some(value),
            get: accessor.then_some(getter),
            set: accessor.then_some(setter),
            writable: (!accessor).then_some(f & qjs::JS_PROP_WRITABLE != 0),
            enumerable: Some(f & qjs::JS_PROP_ENUMERABLE != 0),
            configurable: Some(f & qjs::JS_PROP_CONFIGURABLE != 0),
        }))
    }

    /// `delete obj[key]` (throws when the property is not configurable).
    pub fn delete_property_or_throw<K: Into<PropertyKey>>(&self, key: K, ctx: &mut Context) -> JsResult<bool> {
        let atom = key.into().atom(ctx)?;
        Ok(check_int(ctx.raw, unsafe { qjs::JS_DeleteProperty(ctx.raw, self.0.0, atom.0, qjs::JS_PROP_THROW as i32) })? != 0)
    }

    /// Own property keys (strings in order, then symbols), like `Reflect.ownKeys`.
    pub fn own_property_keys(&self, ctx: &mut Context) -> JsResult<Vec<PropertyKey>> {
        self.own_keys(ctx, (qjs::JS_GPN_STRING_MASK | qjs::JS_GPN_SYMBOL_MASK) as i32)
    }

    /// Own enumerable string keys, like `Object.keys`.
    pub fn own_enumerable_string_keys(&self, ctx: &mut Context) -> JsResult<Vec<PropertyKey>> {
        self.own_keys(ctx, (qjs::JS_GPN_STRING_MASK | qjs::JS_GPN_ENUM_ONLY) as i32)
    }

    fn own_keys(&self, ctx: &mut Context, flags: i32) -> JsResult<Vec<PropertyKey>> {
        let mut tab: *mut qjs::JSPropertyEnum = ptr::null_mut();
        let mut len: u32 = 0;
        check_int(ctx.raw, unsafe { qjs::JS_GetOwnPropertyNames(ctx.raw, &mut tab, &mut len, self.0.0, flags) })?;
        let mut keys = Vec::with_capacity(len as usize);
        for i in 0..len as usize {
            let atom = unsafe { (*tab.add(i)).atom };
            keys.push(PropertyKey::from_atom(ctx.raw, atom));
        }
        unsafe { qjs::JS_FreePropertyEnum(ctx.raw, tab, len) };
        Ok(keys)
    }

    /// Call as a function.
    pub fn call(&self, this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
        let mut argv: Vec<qjs::JSValue> = args.iter().map(|a| a.0).collect();
        let v = unsafe { qjs::JS_Call(ctx.raw, self.0.0, this.0, argv.len() as c_int, argv.as_mut_ptr()) };
        check(ctx.raw, v)
    }

    /// `new obj(...args)` (with an optional `new.target`).
    pub fn construct(&self, args: &[JsValue], new_target: Option<&JsObject>, ctx: &mut Context) -> JsResult<JsObject> {
        let mut argv: Vec<qjs::JSValue> = args.iter().map(|a| a.0).collect();
        let target = new_target.map_or(self.0.0, |t| t.0.0);
        let v = unsafe { qjs::JS_CallConstructor2(ctx.raw, self.0.0, target, argv.len() as c_int, argv.as_mut_ptr()) };
        Ok(JsObject(check(ctx.raw, v)?))
    }

    /// Is callable.
    pub fn is_callable(&self) -> bool {
        self.0.is_callable()
    }

    /// Is a constructor.
    pub fn is_constructor(&self) -> bool {
        self.0.is_constructor()
    }

    /// Is an `Array`.
    pub fn is_array(&self) -> bool {
        unsafe { qjs::JS_IsArray(self.0.0) }
    }

    /// Is an `Error` object.
    pub fn is_error(&self) -> bool {
        unsafe { qjs::JS_IsError(self.0.0) }
    }

    /// `Object.getPrototypeOf(obj)` (`None` for `null`).
    pub fn prototype(&self) -> Option<JsObject> {
        let ctx = main_ctx();
        let v = check(ctx, unsafe { qjs::JS_GetPrototype(ctx, self.0.0) }).ok()?;
        v.as_object()
    }

    /// `Object.setPrototypeOf(obj, proto)`; false when not allowed.
    pub fn set_prototype(&self, proto: Option<JsObject>) -> bool {
        let ctx = main_ctx();
        let p = proto.as_ref().map_or(JsValue::null().0, |p| p.0.0);
        let r = unsafe { qjs::JS_SetPrototype(ctx, self.0.0, p) };
        if r < 0 {
            drop(take_exception_raw(ctx));
            return false;
        }
        r != 0
    }

    /// `Object.freeze(obj)`
    pub fn freeze(&self, ctx: &mut Context) -> JsResult<()> {
        check_int(ctx.raw, unsafe { qjs::JS_FreezeObject(ctx.raw, self.0.0) })?;
        Ok(())
    }

    /// Identity comparison.
    pub fn equals(a: &JsObject, b: &JsObject) -> bool {
        unsafe { qjs::JS_VALUE_GET_PTR(a.0.0) == qjs::JS_VALUE_GET_PTR(b.0.0) }
    }

    /// A stable identity for the lifetime of the object.
    pub fn addr(&self) -> usize {
        unsafe { qjs::JS_VALUE_GET_PTR(self.0.0) as usize }
    }
}

impl From<JsObject> for JsValue {
    fn from(o: JsObject) -> Self {
        o.0
    }
}

impl PartialEq for JsObject {
    fn eq(&self, other: &Self) -> bool {
        JsObject::equals(self, other)
    }
}
impl Eq for JsObject {}

impl std::hash::Hash for JsObject {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr().hash(state)
    }
}

impl fmt::Debug for JsObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JsObject({:#x})", self.addr())
    }
}

// ---------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------

/// A callable object.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JsFunction(JsObject);

impl JsFunction {
    /// The object as a function, if callable.
    pub fn from_object(obj: JsObject) -> Option<JsFunction> {
        obj.is_callable().then_some(JsFunction(obj))
    }
}

impl std::ops::Deref for JsFunction {
    type Target = JsObject;
    fn deref(&self) -> &JsObject {
        &self.0
    }
}

impl From<JsFunction> for JsObject {
    fn from(f: JsFunction) -> Self {
        f.0
    }
}

impl From<JsFunction> for JsValue {
    fn from(f: JsFunction) -> Self {
        f.0.0
    }
}

type NativeFn = dyn Fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>;

/// A Rust function callable from JS. When called with `new`, `this` is `new.target`.
#[derive(Clone)]
pub struct NativeFunction(Rc<NativeFn>);

impl NativeFunction {
    /// From a function pointer.
    pub fn from_fn_ptr(f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>) -> Self {
        NativeFunction(Rc::new(f))
    }

    /// From a closure. JS values captured by the closure are not traced: a cycle through them
    /// leaks (until the engine is dropped).
    ///
    /// # Safety
    /// Kept `unsafe` for parity with the original API; it is memory safe.
    pub unsafe fn from_closure(f: impl Fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue> + 'static) -> Self {
        NativeFunction(Rc::new(f))
    }

    /// From a `Copy` closure.
    pub fn from_copy_closure(f: impl Fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue> + Copy + 'static) -> Self {
        NativeFunction(Rc::new(f))
    }

    /// From a closure with captured state passed by reference.
    pub fn from_copy_closure_with_captures<T: 'static>(
        f: impl Fn(&JsValue, &[JsValue], &T, &mut Context) -> JsResult<JsValue> + Copy + 'static,
        captures: T,
    ) -> Self {
        NativeFunction(Rc::new(move |this: &JsValue, args: &[JsValue], ctx: &mut Context| f(this, args, &captures, ctx)))
    }

    /// From any closure (safe constructor).
    pub fn from_fn(f: impl Fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue> + 'static) -> Self {
        NativeFunction(Rc::new(f))
    }

    /// Build a function object in `realm`.
    pub fn to_js_function(self, realm: &Realm) -> JsFunction {
        FunctionObjectBuilder::new(realm, self).build()
    }
}

struct FnBox(Rc<NativeFn>);

unsafe extern "C" fn fn_finalizer(_rt: *mut qjs::JSRuntime, val: qjs::JSValue) {
    let Some(e) = try_engine() else { return };
    let p = unsafe { qjs::JS_GetOpaque(val, e.fn_class) } as *mut FnBox;
    if !p.is_null() {
        let b = unsafe { Box::from_raw(p) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(b)));
    }
}

unsafe extern "C" fn fn_call(
    ctx: *mut qjs::JSContext,
    func_obj: qjs::JSValue,
    this_val: qjs::JSValue,
    argc: c_int,
    argv: *mut qjs::JSValue,
    _flags: c_int,
) -> qjs::JSValue {
    let e = engine();
    let p = unsafe { qjs::JS_GetOpaque(func_obj, e.fn_class) } as *const FnBox;
    if p.is_null() {
        return throw(ctx, JsNativeError::typ().with_message("not a function").into());
    }
    let f = unsafe { &*p }.0.clone();
    let args: Vec<JsValue> = (0..argc.max(0) as usize).map(|i| unsafe { JsValue::from_raw_dup(*argv.add(i)) }).collect();
    let this = unsafe { JsValue::from_raw_dup(this_val) };
    let mut c = Context::from_raw(ctx);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&this, &args, &mut c)));
    match r {
        Ok(Ok(v)) => v.into_raw(),
        Ok(Err(err)) => throw(ctx, err),
        Err(panic) => {
            let msg = panic_message(&*panic);
            tracing::error!(target: "pi_quickjs", "panic in native function: {msg}");
            throw(ctx, JsNativeError::internal().with_message(format!("panic in native function: {msg}")).into())
        }
    }
}

/// Builds function objects from [`NativeFunction`]s.
pub struct FunctionObjectBuilder {
    realm: Realm,
    f: NativeFunction,
    name: Option<JsString>,
    length: usize,
    constructor: bool,
}

impl FunctionObjectBuilder {
    /// Start a function in `realm`.
    pub fn new(realm: &Realm, f: NativeFunction) -> Self {
        FunctionObjectBuilder { realm: realm.clone(), f, name: None, length: 0, constructor: false }
    }
    /// `name`
    pub fn name(mut self, name: impl Into<JsString>) -> Self {
        self.name = Some(name.into());
        self
    }
    /// `length`
    pub fn length(mut self, length: usize) -> Self {
        self.length = length;
        self
    }
    /// Whether `new` is allowed.
    pub fn constructor(mut self, yes: bool) -> Self {
        self.constructor = yes;
        self
    }
    /// Create the function.
    pub fn build(self) -> JsFunction {
        let e = engine();
        let raw = self.realm.0;
        let proto = unsafe { JsValue::from_raw(qjs::JS_GetFunctionProto(raw)) };
        let v = check(raw, unsafe { qjs::JS_NewObjectProtoClass(raw, proto.0, e.fn_class) }).expect("out of memory");
        let b = Box::new(FnBox(self.f.0));
        unsafe {
            qjs::JS_SetOpaque(v.0, Box::into_raw(b) as *mut c_void);
            if self.constructor {
                qjs::JS_SetConstructorBit(raw, v.0, true);
            }
        }
        let obj = JsObject(v);
        let mut ctx = Context::from_raw(raw);
        let name = self.name.unwrap_or_else(|| JsString::from(""));
        let desc = |v: JsValue| PropertyDescriptor::builder().value(v).writable(false).enumerable(false).configurable(true);
        let _ = obj.define_property_or_throw("length", desc(JsValue::from(self.length as f64)), &mut ctx);
        let _ = obj.define_property_or_throw("name", desc(name.into()), &mut ctx);
        JsFunction(obj)
    }
}

// ---------------------------------------------------------------------------
// Arrays, buffers, typed arrays, promises
// ---------------------------------------------------------------------------

/// A JS `Array`.
#[derive(Clone, Debug, PartialEq)]
pub struct JsArray(JsObject);

impl JsArray {
    /// `[]`
    pub fn new(ctx: &mut Context) -> JsArray {
        JsArray(JsObject(check(ctx.raw, unsafe { qjs::JS_NewArray(ctx.raw) }).expect("out of memory")))
    }

    /// An array of `items`.
    pub fn from_iter<I: IntoIterator<Item = JsValue>>(items: I, ctx: &mut Context) -> JsArray {
        let arr = JsArray::new(ctx);
        for (i, v) in items.into_iter().enumerate() {
            let _ = arr.0.create_data_property_or_throw(i, v, ctx);
        }
        arr
    }

    /// The object as an array.
    pub fn from_object(obj: JsObject) -> JsResult<JsArray> {
        if obj.is_array() {
            Ok(JsArray(obj))
        } else {
            Err(JsNativeError::typ().with_message("object is not an Array").into())
        }
    }

    /// `array.length`
    pub fn length(&self, ctx: &mut Context) -> JsResult<u64> {
        let mut n = 0i64;
        check_int(ctx.raw, unsafe { qjs::JS_GetLength(ctx.raw, self.0.0.0, &mut n) })?;
        Ok(n.max(0) as u64)
    }

    /// `array[index]`
    pub fn get<I: Into<PropertyKey>>(&self, index: I, ctx: &mut Context) -> JsResult<JsValue> {
        self.0.get(index, ctx)
    }

    /// `array.at(index)`
    pub fn at<I: Into<i64>>(&self, index: I, ctx: &mut Context) -> JsResult<JsValue> {
        let i = index.into();
        let i = if i < 0 { self.length(ctx)? as i64 + i } else { i };
        if i < 0 { Ok(JsValue::undefined()) } else { self.0.get(i, ctx) }
    }

    /// `array.push(value)`
    pub fn push<V: Into<JsValue>>(&self, value: V, ctx: &mut Context) -> JsResult<JsValue> {
        let len = self.length(ctx)?;
        self.0.set(len, value, true, ctx)?;
        Ok(JsValue::from((len + 1) as f64))
    }

    /// The elements as a vector.
    pub fn to_vec(&self, ctx: &mut Context) -> JsResult<Vec<JsValue>> {
        let len = self.length(ctx)?;
        (0..len).map(|i| self.0.get(i, ctx)).collect()
    }
}

impl std::ops::Deref for JsArray {
    type Target = JsObject;
    fn deref(&self) -> &JsObject {
        &self.0
    }
}

impl From<JsArray> for JsValue {
    fn from(a: JsArray) -> Self {
        a.0.0
    }
}

impl From<JsArray> for JsObject {
    fn from(a: JsArray) -> Self {
        a.0
    }
}

/// A JS `ArrayBuffer`.
#[derive(Clone, Debug, PartialEq)]
pub struct JsArrayBuffer(JsObject);

unsafe extern "C" fn vec_realloc(_rt: *mut qjs::JSRuntime, opaque: *mut c_void, ptr: *mut c_void, size: qjs::size_t) -> *mut c_void {
    // Used for buffers created from a `Vec<u8>`: `opaque` is the original capacity. Only frees
    // (size 0) are expected since these buffers are not resizable.
    if size == 0 {
        if !ptr.is_null() {
            let cap = opaque as usize;
            drop(unsafe { Vec::from_raw_parts(ptr as *mut u8, 0, cap) });
        }
        return ptr::null_mut();
    }
    ptr::null_mut()
}

unsafe extern "C" fn external_realloc(_rt: *mut qjs::JSRuntime, _opaque: *mut c_void, _ptr: *mut c_void, _size: qjs::size_t) -> *mut c_void {
    // External memory is owned by Rust: nothing to free, and it is never resized.
    ptr::null_mut()
}

impl JsArrayBuffer {
    /// A buffer viewing `len` bytes of memory owned by Rust (never freed or resized by JS).
    ///
    /// # Safety
    /// `ptr..ptr + len` must stay valid while the buffer is attached; detach it (see
    /// [`JsArrayBuffer::detach_in_place`]) or drop the engine before freeing the memory.
    pub unsafe fn from_external(ptr: *mut u8, len: usize, ctx: &mut Context) -> JsResult<JsArrayBuffer> {
        let v = unsafe { qjs::JS_NewArrayBuffer(ctx.raw, ptr, len as _, len as _, Some(external_realloc as _), ptr::null_mut(), false) };
        Ok(JsArrayBuffer(JsObject(check(ctx.raw, v)?)))
    }

    /// Detach the buffer without copying its bytes.
    pub fn detach_in_place(&self) {
        unsafe { qjs::JS_DetachArrayBuffer(main_ctx(), self.0.0.0) };
    }

    /// A zero-filled buffer.
    pub fn new(len: usize, ctx: &mut Context) -> JsResult<JsArrayBuffer> {
        Self::from_byte_block(vec![0u8; len], ctx)
    }

    /// A buffer owning `bytes` (no copy).
    pub fn from_byte_block(bytes: Vec<u8>, ctx: &mut Context) -> JsResult<JsArrayBuffer> {
        if bytes.is_empty() {
            let v = check(ctx.raw, unsafe { qjs::JS_NewArrayBufferCopy(ctx.raw, ptr::null(), 0) })?;
            return Ok(JsArrayBuffer(JsObject(v)));
        }
        let mut bytes = std::mem::ManuallyDrop::new(bytes);
        let (p, len, cap) = (bytes.as_mut_ptr(), bytes.len(), bytes.capacity());
        let v = unsafe { qjs::JS_NewArrayBuffer(ctx.raw, p, len as _, len as _, Some(vec_realloc as _), cap as *mut c_void, false) };
        match check(ctx.raw, v) {
            Ok(v) => Ok(JsArrayBuffer(JsObject(v))),
            Err(e) => {
                drop(unsafe { Vec::from_raw_parts(p, len, cap) });
                Err(e)
            }
        }
    }

    /// A copy of `bytes`.
    pub fn from_slice(bytes: &[u8], ctx: &mut Context) -> JsResult<JsArrayBuffer> {
        let v = check(ctx.raw, unsafe { qjs::JS_NewArrayBufferCopy(ctx.raw, bytes.as_ptr(), bytes.len() as _) })?;
        Ok(JsArrayBuffer(JsObject(v)))
    }

    /// The object as an ArrayBuffer.
    pub fn from_object(obj: JsObject) -> JsResult<JsArrayBuffer> {
        if unsafe { qjs::JS_IsArrayBuffer(obj.0.0) } {
            Ok(JsArrayBuffer(obj))
        } else {
            Err(JsNativeError::typ().with_message("object is not an ArrayBuffer").into())
        }
    }

    fn raw_data(&self) -> Option<(*mut u8, usize)> {
        let ctx = main_ctx();
        let mut len: qjs::size_t = 0;
        let p = unsafe { qjs::JS_GetArrayBuffer(ctx, &mut len, self.0.0.0) };
        let len = len as usize;
        if p.is_null() {
            if unsafe { qjs::JS_HasException(ctx) } {
                drop(take_exception_raw(ctx));
                return None;
            }
            return Some((ptr::NonNull::dangling().as_ptr(), 0));
        }
        Some((p, len))
    }

    /// The bytes (`None` when detached). The slice must not be held across JS calls that may
    /// detach or resize the buffer.
    pub fn data(&self) -> Option<&[u8]> {
        let (p, len) = self.raw_data()?;
        Some(unsafe { std::slice::from_raw_parts(p, len) })
    }

    /// The bytes, mutably (`None` when detached). Same caveat as [`JsArrayBuffer::data`].
    #[allow(clippy::mut_from_ref)]
    pub fn data_mut(&self) -> Option<&mut [u8]> {
        let (p, len) = self.raw_data()?;
        Some(unsafe { std::slice::from_raw_parts_mut(p, len) })
    }

    /// Whether the buffer is detached.
    pub fn is_detached(&self) -> bool {
        self.raw_data().is_none()
    }

    /// `byteLength`
    pub fn byte_length(&self) -> usize {
        self.raw_data().map_or(0, |(_, l)| l)
    }

    /// Detach the buffer, returning its bytes.
    pub fn detach(&self, _key: &JsValue) -> JsResult<Vec<u8>> {
        let data = self.data().map(|d| d.to_vec()).ok_or_else(|| JsNativeError::typ().with_message("ArrayBuffer is detached"))?;
        unsafe { qjs::JS_DetachArrayBuffer(main_ctx(), self.0.0.0) };
        Ok(data)
    }

    /// A resizable copy with `maxByteLength` (via the `ArrayBuffer` constructor).
    pub fn with_max_byte_length(self, max: u64, ctx: &mut Context) -> JsResult<JsArrayBuffer> {
        let data = self.data().map(|d| d.to_vec()).unwrap_or_default();
        let ctor = ctx.global_object().get("ArrayBuffer", ctx)?;
        let ctor = ctor.as_object().ok_or_else(|| JsNativeError::typ().with_message("ArrayBuffer missing"))?;
        let opts = JsObject::with_object_proto(ctx.intrinsics());
        opts.set("maxByteLength", JsValue::from(max as f64), true, ctx)?;
        let ab = JsArrayBuffer::from_object(ctor.construct(&[JsValue::from(data.len() as f64), opts.into()], None, ctx)?)?;
        if let Some(d) = ab.data_mut() {
            d.copy_from_slice(&data);
        }
        Ok(ab)
    }
}

impl std::ops::Deref for JsArrayBuffer {
    type Target = JsObject;
    fn deref(&self) -> &JsObject {
        &self.0
    }
}

impl From<JsArrayBuffer> for JsValue {
    fn from(a: JsArrayBuffer) -> Self {
        a.0.0
    }
}

impl From<JsArrayBuffer> for JsObject {
    fn from(a: JsArrayBuffer) -> Self {
        a.0
    }
}

/// The element type of a typed array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypedArrayKind {
    /// `Uint8ClampedArray`
    Uint8Clamped,
    /// `Int8Array`
    Int8,
    /// `Uint8Array`
    Uint8,
    /// `Int16Array`
    Int16,
    /// `Uint16Array`
    Uint16,
    /// `Int32Array`
    Int32,
    /// `Uint32Array`
    Uint32,
    /// `BigInt64Array`
    BigInt64,
    /// `BigUint64Array`
    BigUint64,
    /// `Float16Array`
    Float16,
    /// `Float32Array`
    Float32,
    /// `Float64Array`
    Float64,
}

impl TypedArrayKind {
    fn from_raw(n: c_int) -> Option<Self> {
        use TypedArrayKind::*;
        Some(match n {
            0 => Uint8Clamped,
            1 => Int8,
            2 => Uint8,
            3 => Int16,
            4 => Uint16,
            5 => Int32,
            6 => Uint32,
            7 => BigInt64,
            8 => BigUint64,
            9 => Float16,
            10 => Float32,
            11 => Float64,
            _ => return None,
        })
    }

    /// The constructor name.
    pub fn name(self) -> &'static str {
        use TypedArrayKind::*;
        match self {
            Uint8Clamped => "Uint8ClampedArray",
            Int8 => "Int8Array",
            Uint8 => "Uint8Array",
            Int16 => "Int16Array",
            Uint16 => "Uint16Array",
            Int32 => "Int32Array",
            Uint32 => "Uint32Array",
            BigInt64 => "BigInt64Array",
            BigUint64 => "BigUint64Array",
            Float16 => "Float16Array",
            Float32 => "Float32Array",
            Float64 => "Float64Array",
        }
    }

    /// Bytes per element.
    pub fn element_size(self) -> usize {
        use TypedArrayKind::*;
        match self {
            Uint8Clamped | Int8 | Uint8 => 1,
            Int16 | Uint16 | Float16 => 2,
            Int32 | Uint32 | Float32 => 4,
            BigInt64 | BigUint64 | Float64 => 8,
        }
    }
}

/// A typed array of any kind.
#[derive(Clone, Debug, PartialEq)]
pub struct JsTypedArray(JsObject);

impl JsTypedArray {
    /// The object as a typed array.
    pub fn from_object(obj: JsObject) -> JsResult<JsTypedArray> {
        if unsafe { qjs::JS_GetTypedArrayType(obj.0.0) } >= 0 {
            Ok(JsTypedArray(obj))
        } else {
            Err(JsNativeError::typ().with_message("object is not a TypedArray").into())
        }
    }

    /// The element type.
    pub fn kind(&self) -> Option<TypedArrayKind> {
        TypedArrayKind::from_raw(unsafe { qjs::JS_GetTypedArrayType(self.0.0.0) })
    }

    fn parts(&self, ctx: &mut Context) -> JsResult<(JsValue, usize, usize)> {
        let (mut off, mut len, mut bpe): (qjs::size_t, qjs::size_t, qjs::size_t) = (0, 0, 0);
        let ab = check(ctx.raw, unsafe { qjs::JS_GetTypedArrayBuffer(ctx.raw, self.0.0.0, &mut off, &mut len, &mut bpe) })?;
        Ok((ab, off as usize, len as usize))
    }

    /// `ta.buffer`
    pub fn buffer(&self, ctx: &mut Context) -> JsResult<JsValue> {
        Ok(self.parts(ctx)?.0)
    }

    /// `ta.byteOffset`
    pub fn byte_offset(&self, ctx: &mut Context) -> JsResult<usize> {
        let v = self.0.get("byteOffset", ctx)?;
        Ok(v.to_number(ctx)? as usize)
    }

    /// `ta.byteLength`
    pub fn byte_length(&self, ctx: &mut Context) -> JsResult<usize> {
        let v = self.0.get("byteLength", ctx)?;
        Ok(v.to_number(ctx)? as usize)
    }

    /// `ta.length`
    pub fn length(&self, ctx: &mut Context) -> JsResult<usize> {
        let v = self.0.get("length", ctx)?;
        Ok(v.to_number(ctx)? as usize)
    }

    /// A copy of the viewed bytes (`None` when detached).
    pub fn to_bytes(&self, ctx: &mut Context) -> JsResult<Option<Vec<u8>>> {
        let (ab, _, _) = self.parts(ctx)?;
        let off = self.byte_offset(ctx)?;
        let len = self.byte_length(ctx)?;
        let Some(ab) = ab.as_object().and_then(|o| JsArrayBuffer::from_object(o).ok()) else { return Ok(None) };
        Ok(ab.data().and_then(|d| d.get(off..off + len)).map(|s| s.to_vec()))
    }
}

impl std::ops::Deref for JsTypedArray {
    type Target = JsObject;
    fn deref(&self) -> &JsObject {
        &self.0
    }
}

impl From<JsTypedArray> for JsValue {
    fn from(a: JsTypedArray) -> Self {
        a.0.0
    }
}

impl From<JsTypedArray> for JsObject {
    fn from(a: JsTypedArray) -> Self {
        a.0
    }
}

/// A `Uint8Array`.
#[derive(Clone, Debug, PartialEq)]
pub struct JsUint8Array(JsTypedArray);

impl JsUint8Array {
    /// A view of the whole buffer.
    pub fn from_array_buffer(ab: JsArrayBuffer, ctx: &mut Context) -> JsResult<JsUint8Array> {
        let mut argv = [ab.0.0.0];
        let v = check(ctx.raw, unsafe { qjs::JS_NewTypedArray(ctx.raw, 1, argv.as_mut_ptr(), qjs::JSTypedArrayEnum_JS_TYPED_ARRAY_UINT8) })?;
        Ok(JsUint8Array(JsTypedArray(JsObject(v))))
    }

    /// A new array holding `bytes`.
    pub fn from_iter<I: IntoIterator<Item = u8>>(bytes: I, ctx: &mut Context) -> JsResult<JsUint8Array> {
        let bytes: Vec<u8> = bytes.into_iter().collect();
        Self::from_vec(bytes, ctx)
    }

    /// A new array owning `bytes`.
    pub fn from_vec(bytes: Vec<u8>, ctx: &mut Context) -> JsResult<JsUint8Array> {
        let ab = JsArrayBuffer::from_byte_block(bytes, ctx)?;
        Self::from_array_buffer(ab, ctx)
    }

    /// A copy of the bytes (empty when detached).
    pub fn to_vec(&self, ctx: &mut Context) -> JsResult<Vec<u8>> {
        Ok(self.0.to_bytes(ctx)?.unwrap_or_default())
    }

    /// The object as a Uint8Array.
    pub fn from_object(obj: JsObject) -> JsResult<JsUint8Array> {
        let ta = JsTypedArray::from_object(obj)?;
        if ta.kind() == Some(TypedArrayKind::Uint8) {
            Ok(JsUint8Array(ta))
        } else {
            Err(JsNativeError::typ().with_message("object is not a Uint8Array").into())
        }
    }
}

impl std::ops::Deref for JsUint8Array {
    type Target = JsTypedArray;
    fn deref(&self) -> &JsTypedArray {
        &self.0
    }
}

impl From<JsUint8Array> for JsValue {
    fn from(a: JsUint8Array) -> Self {
        a.0.0.0
    }
}

impl From<JsUint8Array> for JsObject {
    fn from(a: JsUint8Array) -> Self {
        a.0.0
    }
}

/// A `DataView`.
#[derive(Clone, Debug, PartialEq)]
pub struct JsDataView(JsObject);

impl JsDataView {
    /// The object as a DataView.
    pub fn from_object(obj: JsObject) -> JsResult<JsDataView> {
        if unsafe { qjs::JS_IsDataView(obj.0.0) } {
            Ok(JsDataView(obj))
        } else {
            Err(JsNativeError::typ().with_message("object is not a DataView").into())
        }
    }
    /// `dv.buffer`
    pub fn buffer(&self, ctx: &mut Context) -> JsResult<JsValue> {
        self.0.get("buffer", ctx)
    }
    /// `dv.byteOffset`
    pub fn byte_offset(&self, ctx: &mut Context) -> JsResult<u64> {
        Ok(self.0.get("byteOffset", ctx)?.to_number(ctx)? as u64)
    }
    /// `dv.byteLength`
    pub fn byte_length(&self, ctx: &mut Context) -> JsResult<u64> {
        Ok(self.0.get("byteLength", ctx)?.to_number(ctx)? as u64)
    }
}

impl std::ops::Deref for JsDataView {
    type Target = JsObject;
    fn deref(&self) -> &JsObject {
        &self.0
    }
}

/// The state of a promise.
#[derive(Debug, Clone)]
pub enum PromiseState {
    /// Not settled.
    Pending,
    /// Fulfilled with a value.
    Fulfilled(JsValue),
    /// Rejected with a reason.
    Rejected(JsValue),
}

/// The `resolve` / `reject` functions of a pending promise.
#[derive(Clone, Debug)]
pub struct ResolvingFunctions {
    /// `resolve`
    pub resolve: JsFunction,
    /// `reject`
    pub reject: JsFunction,
}

/// A `Promise`.
#[derive(Clone, Debug, PartialEq)]
pub struct JsPromise(JsObject);

impl JsPromise {
    /// A pending promise and its resolving functions.
    pub fn new_pending(ctx: &mut Context) -> (JsPromise, ResolvingFunctions) {
        let mut funcs = [JsValue::undefined().0, JsValue::undefined().0];
        let p = check(ctx.raw, unsafe { qjs::JS_NewPromiseCapability(ctx.raw, funcs.as_mut_ptr()) }).expect("out of memory");
        let (resolve, reject) = unsafe { (JsValue::from_raw(funcs[0]), JsValue::from_raw(funcs[1])) };
        (
            JsPromise(JsObject(p)),
            ResolvingFunctions { resolve: JsFunction(JsObject(resolve)), reject: JsFunction(JsObject(reject)) },
        )
    }

    /// The object as a promise.
    pub fn from_object(obj: JsObject) -> JsResult<JsPromise> {
        if unsafe { qjs::JS_IsPromise(obj.0.0) } {
            Ok(JsPromise(obj))
        } else {
            Err(JsNativeError::typ().with_message("object is not a Promise").into())
        }
    }

    /// A promise resolved with `value`.
    pub fn resolve(value: impl Into<JsValue>, ctx: &mut Context) -> JsResult<JsPromise> {
        let (p, f) = JsPromise::new_pending(ctx);
        f.resolve.call(&JsValue::undefined(), &[value.into()], ctx)?;
        Ok(p)
    }

    /// A promise rejected with `err`.
    pub fn reject(err: impl Into<JsError>, ctx: &mut Context) -> JsResult<JsPromise> {
        let reason = err.into().to_opaque(ctx);
        let (p, f) = JsPromise::new_pending(ctx);
        f.reject.call(&JsValue::undefined(), &[reason], ctx)?;
        Ok(p)
    }

    /// The current state.
    pub fn state(&self) -> PromiseState {
        let ctx = main_ctx();
        let s = unsafe { qjs::JS_PromiseState(ctx, self.0.0.0) };
        match s {
            qjs::JSPromiseStateEnum_JS_PROMISE_FULFILLED => {
                PromiseState::Fulfilled(unsafe { JsValue::from_raw(qjs::JS_PromiseResult(ctx, self.0.0.0)) })
            }
            qjs::JSPromiseStateEnum_JS_PROMISE_REJECTED => {
                PromiseState::Rejected(unsafe { JsValue::from_raw(qjs::JS_PromiseResult(ctx, self.0.0.0)) })
            }
            _ => PromiseState::Pending,
        }
    }

    /// `promise.then(onFulfilled, onRejected)`
    pub fn then(&self, on_fulfilled: Option<JsFunction>, on_rejected: Option<JsFunction>, ctx: &mut Context) -> JsResult<JsPromise> {
        let then = ctx.intrinsics().constructors().promise().prototype().get("then", ctx)?;
        let then = then.as_object().ok_or_else(|| JsNativeError::typ().with_message("Promise.prototype.then missing"))?;
        let args = [
            on_fulfilled.map_or_else(JsValue::undefined, JsValue::from),
            on_rejected.map_or_else(JsValue::undefined, JsValue::from),
        ];
        let r = then.call(self.0.as_value(), &args, ctx)?;
        Ok(r.as_promise().unwrap_or_else(|| self.clone()))
    }
}

impl std::ops::Deref for JsPromise {
    type Target = JsObject;
    fn deref(&self) -> &JsObject {
        &self.0
    }
}

impl From<JsPromise> for JsValue {
    fn from(p: JsPromise) -> Self {
        p.0.0
    }
}

impl From<JsPromise> for JsObject {
    fn from(p: JsPromise) -> Self {
        p.0
    }
}
