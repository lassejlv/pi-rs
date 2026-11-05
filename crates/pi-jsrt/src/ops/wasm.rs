//! The `WebAssembly` JS API on the pure-Rust wasmi interpreter (glue: `js/web/wasm.js`).
//!
//! # Store
//!
//! Every runtime thread has one wasmi `Store` holding all modules, instances, memories,
//! tables and globals created by JS (imports between instances need a shared store). It lives
//! until the runtime shuts down.
//!
//! # Memory shared with JS (zero copy)
//!
//! JS needs `memory.buffer` to be an `ArrayBuffer` that aliases wasm linear memory (emscripten
//! and undici write request bytes into it and read results out of it). wasmi can run a memory on a
//! caller-provided static buffer, and a QuickJS ArrayBuffer can view external memory. So each
//! memory is one allocation reserved up to its maximum size (4 GiB when unbounded; virtual memory,
//! only touched pages are committed; wasmi zeroes the live part), owned by this module until the
//! runtime shuts down: wasmi gets the whole reservation as its static buffer, and the JS
//! ArrayBuffer is an external view of it with `len` = the current size. When wasm grows the memory
//! the next `memory.buffer` read detaches the old ArrayBuffer (as V8 does) and views the same
//! allocation, with the new length, in a new one. Memories a module defines itself are turned
//! into imports by rewriting the module's import/memory sections, so they get the same backing.
//! Current buffers are rooted in this module's state for the store's lifetime.
//!
//! Caveat: Node forbids transferring a memory's buffer from JS (`ArrayBuffer.prototype.transfer`)
//! with a detach key; here a transfer detaches the view, and the next `memory.buffer` read fails.
//! Shared memories are backed by plain ArrayBuffers.
//!
//! # Calls
//!
//! JS → wasm calls borrow the store; wasm → JS host calls receive a wasmi `Caller`, which is
//! pushed on a stack so that JS running inside a host call (re-entering wasm, reading
//! `memory.buffer`, ...) uses the caller's store context instead of the borrowed store. A JS
//! exception thrown in a host function unwinds wasm as a host error and is rethrown unchanged
//! to the JS caller.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::js::FunctionObjectBuilder;
use crate::js::{JsArray, JsArrayBuffer};
use crate::js::PropertyDescriptor;
use crate::js::{Context, JsArgs, JsBigInt, JsError, JsObject, JsResult, JsSymbol, JsValue, NativeFunction, js_string};
use wasmi::{AsContextMut, Caller, Extern, ExternRef, ExternType, Func, FuncType, Global, Memory, MemoryType, Module, Mutability, Nullable, Ref, RefType, StoreContextMut, Table, TableType, Val, ValType};

use super::{bytes_arg, native_object, object, with_native};
use crate::engine::{self, OpFn, arg_f64, arg_string, js_str, type_error};

/// Module name of the imports that replace memories a module defines itself.
const SYNTHETIC_MODULE: &str = "\u{1}pi-jsrt";
const PAGE: usize = 65536;
const ALIGN: usize = 64;

struct MemSlot {
    mem: Memory,
    buffer: JsArrayBuffer,
    len: usize,
    ptr: *mut u8,
    cap: usize,
}

struct Wasm {
    engine: wasmi::Engine,
    store: RefCell<wasmi::Store<()>>,
    /// Active host-call `Caller`s (innermost last), as `*mut Caller<'static, ()>`.
    callers: RefCell<Vec<*mut ()>>,
    /// Contexts of the JS frames that entered wasm.
    ctxs: RefCell<Vec<*mut Context>>,
    js_funcs: RefCell<Vec<JsObject>>,
    externs: RefCell<Vec<JsValue>>,
    func_objects: RefCell<HashMap<String, JsObject>>,
    memories: RefCell<HashMap<String, MemSlot>>,
    pending: RefCell<Option<JsError>>,
    func_symbol: JsSymbol,
}

thread_local! {
    static WASM: RefCell<Option<Rc<Wasm>>> = const { RefCell::new(None) };
}

fn state() -> Rc<Wasm> {
    WASM.with(|w| {
        w.borrow_mut()
            .get_or_insert_with(|| {
                let engine = wasmi::Engine::default();
                Rc::new(Wasm {
                    store: RefCell::new(wasmi::Store::new(&engine, ())),
                    engine,
                    callers: RefCell::new(Vec::new()),
                    ctxs: RefCell::new(Vec::new()),
                    js_funcs: RefCell::new(Vec::new()),
                    externs: RefCell::new(Vec::new()),
                    func_objects: RefCell::new(HashMap::new()),
                    memories: RefCell::new(HashMap::new()),
                    pending: RefCell::new(None),
                    func_symbol: JsSymbol::new(Some(js_string!("wasmFunc"))).expect("symbol"),
                })
            })
            .clone()
    })
}

/// Drop the store and every JS value it roots (runtime shutdown, before the context).
pub(crate) fn teardown(_ctx: &mut Context) {
    let w = WASM.with(|w| w.borrow_mut().take());
    if let Some(w) = w {
        // The store never frees static memory buffers: free them once the store is gone (their
        // ArrayBuffers are external views that JS can no longer reach).
        let allocations: Vec<(*mut u8, usize)> = w.memories.borrow().values().map(|slot| (slot.ptr, slot.cap)).collect();
        for slot in w.memories.borrow().values() {
            slot.buffer.detach_in_place();
        }
        w.memories.borrow_mut().clear();
        w.func_objects.borrow_mut().clear();
        w.js_funcs.borrow_mut().clear();
        w.externs.borrow_mut().clear();
        drop(w);
        for (ptr, cap) in allocations {
            // SAFETY: allocated by `new_memory` with this layout; the store that used it is gone.
            unsafe { std::alloc::dealloc(ptr, std::alloc::Layout::from_size_align_unchecked(cap, ALIGN)) };
        }
    }
}

/// Run `f` with the current store context: the innermost host call's caller, or the store.
fn with_store<R>(w: &Wasm, f: impl FnOnce(StoreContextMut<'_, ()>) -> R) -> R {
    let top = w.callers.borrow().last().copied();
    match top {
        Some(ptr) => {
            // SAFETY: the pointer was pushed by `host_call` for a `Caller` that is alive on its
            // stack frame until it pops it; we are running inside that call (single thread).
            let caller = unsafe { &mut *(ptr as *mut Caller<'static, ()>) };
            f(caller.as_context_mut())
        }
        None => {
            let mut store = w.store.borrow_mut();
            f(store.as_context_mut())
        }
    }
}

fn key<T: std::fmt::Debug>(h: &T) -> String {
    format!("{h:?}")
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A `WebAssembly.CompileError` / `LinkError` / `RuntimeError` (constructors from the glue).
fn wasm_error(ctx: &mut Context, kind: &str, message: &str) -> JsError {
    let ctor = engine::try_internal(ctx)
        .and_then(|i| i.get(js_string!("wasmErrors"), ctx).ok())
        .and_then(|e| e.as_object().and_then(|o| o.get(js_str(kind).as_string().unwrap(), ctx).ok()))
        .and_then(|c| c.as_object().filter(|o| o.is_constructor()));
    match ctor {
        Some(c) => match c.construct(&[js_str(message)], None, ctx) {
            Ok(obj) => JsError::from_opaque(obj.into()),
            Err(e) => e,
        },
        None => crate::js::JsNativeError::error().with_message(format!("{kind}: {message}")).into(),
    }
}

fn trap_error(ctx: &mut Context, e: &wasmi::Error) -> JsError {
    use wasmi::TrapCode as T;
    let message = match e.as_trap_code() {
        Some(T::UnreachableCodeReached) => "unreachable",
        Some(T::MemoryOutOfBounds) => "memory access out of bounds",
        Some(T::TableOutOfBounds) => "table index is out of bounds",
        Some(T::IndirectCallToNull) | Some(T::BadSignature) => "null function or function signature mismatch",
        Some(T::IntegerDivisionByZero) => "divide by zero",
        Some(T::IntegerOverflow) => "divide result unrepresentable",
        Some(T::BadConversionToInteger) => "float unrepresentable in integer range",
        Some(T::StackOverflow) => {
            return crate::js::JsNativeError::range().with_message("Maximum call stack size exceeded").into();
        }
        _ => return wasm_error(ctx, "RuntimeError", &e.to_string()),
    };
    wasm_error(ctx, "RuntimeError", message)
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

fn val_to_js(w: &Wasm, v: &Val, ctx: &mut Context) -> JsResult<JsValue> {
    Ok(match v {
        Val::I32(n) => JsValue::from(*n),
        Val::I64(n) => JsValue::from(JsBigInt::from(*n)),
        Val::F32(f) => JsValue::from(f.to_float() as f64),
        Val::F64(f) => JsValue::from(f.to_float()),
        Val::FuncRef(Nullable::Val(f)) => export_function(w, *f, None, ctx)?.into(),
        Val::ExternRef(Nullable::Val(r)) => extern_value(w, r),
        Val::FuncRef(Nullable::Null) | Val::ExternRef(Nullable::Null) => JsValue::null(),
        Val::V128(_) => return Err(type_error("type incompatibility when transforming from/to JS")),
    })
}

fn extern_value(w: &Wasm, r: &ExternRef) -> JsValue {
    let idx = with_store(w, |s| r.data(&s).downcast_ref::<usize>().copied());
    idx.and_then(|i| w.externs.borrow().get(i).cloned()).unwrap_or_default()
}

fn wasm_func_of(w: &Wasm, v: &JsValue, ctx: &mut Context) -> JsResult<Option<Func>> {
    let Some(obj) = v.as_object() else { return Ok(None) };
    let handle = obj.get(w.func_symbol.clone(), ctx)?;
    if handle.is_undefined() {
        return Ok(None);
    }
    with_native::<Func, _>(&handle, |f| *f).map(Some)
}

fn js_to_ref(w: &Wasm, v: &JsValue, ty: RefType, ctx: &mut Context) -> JsResult<Ref> {
    match ty {
        RefType::Func => {
            if v.is_null() {
                return Ok(Ref::Func(Nullable::Null));
            }
            match wasm_func_of(w, v, ctx)? {
                Some(f) => Ok(Ref::Func(Nullable::Val(f))),
                None => Err(type_error("type incompatibility when transforming from/to JS")),
            }
        }
        RefType::Extern => {
            if v.is_null() {
                return Ok(Ref::Extern(Nullable::Null));
            }
            let idx = {
                let mut ex = w.externs.borrow_mut();
                ex.push(v.clone());
                ex.len() - 1
            };
            let r = with_store(w, |s| ExternRef::new(s, idx));
            Ok(Ref::Extern(Nullable::Val(r)))
        }
    }
}

fn js_to_val(w: &Wasm, v: &JsValue, ty: ValType, ctx: &mut Context) -> JsResult<Val> {
    Ok(match ty {
        ValType::I32 => Val::I32(v.to_i32(ctx)?),
        ValType::I64 => Val::I64(v.to_big_int64(ctx)?),
        ValType::F32 => Val::F32((v.to_number(ctx)? as f32).into()),
        ValType::F64 => Val::F64(v.to_number(ctx)?.into()),
        ValType::FuncRef => match js_to_ref(w, v, RefType::Func, ctx)? {
            Ref::Func(f) => Val::FuncRef(f),
            _ => unreachable!(),
        },
        ValType::ExternRef => match js_to_ref(w, v, RefType::Extern, ctx)? {
            Ref::Extern(r) => Val::ExternRef(r),
            _ => unreachable!(),
        },
        ValType::V128 => return Err(type_error("type incompatibility when transforming from/to JS")),
    })
}

fn ref_to_js(w: &Wasm, r: Ref, ctx: &mut Context) -> JsResult<JsValue> {
    match r {
        Ref::Func(f) => val_to_js(w, &Val::FuncRef(f), ctx),
        Ref::Extern(e) => val_to_js(w, &Val::ExternRef(e), ctx),
    }
}

fn results_to_js(w: &Wasm, results: &[Val], ctx: &mut Context) -> JsResult<JsValue> {
    match results.len() {
        0 => Ok(JsValue::undefined()),
        1 => val_to_js(w, &results[0], ctx),
        _ => {
            let mut items = Vec::with_capacity(results.len());
            for r in results {
                items.push(val_to_js(w, r, ctx)?);
            }
            Ok(JsArray::from_iter(items, ctx).into())
        }
    }
}

// ---------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------

/// Call a wasm function from JS.
fn call_wasm(func: Func, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let w = state();
    let ty = with_store(&w, |s| func.ty(s));
    let mut params = Vec::with_capacity(ty.params().len());
    for (i, t) in ty.params().iter().enumerate() {
        params.push(js_to_val(&w, args.get_or_undefined(i), *t, ctx)?);
    }
    let mut results: Vec<Val> = ty.results().iter().map(|t| Val::default_for_ty(*t)).collect();
    w.ctxs.borrow_mut().push(ctx as *mut Context);
    let r = with_store(&w, |s| func.call(s, &params, &mut results));
    w.ctxs.borrow_mut().pop();
    refresh_memories(&w, ctx)?;
    match r {
        Ok(()) => results_to_js(&w, &results, ctx),
        Err(e) => match w.pending.borrow_mut().take() {
            Some(js) => Err(js),
            None => Err(trap_error(ctx, &e)),
        },
    }
}

/// The JS function for a wasm function (one object per function, like V8).
fn export_function(w: &Wasm, func: Func, name: Option<&str>, ctx: &mut Context) -> JsResult<JsObject> {
    let k = key(&func);
    if let Some(f) = w.func_objects.borrow().get(&k) {
        return Ok(f.clone());
    }
    let nparams = with_store(w, |s| func.ty(s).params().len());
    let native = NativeFunction::from_copy_closure(move |_, args, ctx| call_wasm(func, args, ctx));
    let realm = ctx.realm().clone();
    let obj = FunctionObjectBuilder::new(&realm, native).name(js_str(name.unwrap_or("0")).to_string(ctx)?).length(nparams).build();
    obj.define_property_or_throw(
        w.func_symbol.clone(),
        PropertyDescriptor::builder().value(native_object(func)).writable(false).enumerable(false).configurable(false),
        ctx,
    )?;
    let obj: JsObject = obj.into();
    w.func_objects.borrow_mut().insert(k, obj.clone());
    Ok(obj)
}

/// A wasm → JS call of the imported JS function `idx`.
fn host_call(idx: usize, result_types: &[ValType], mut caller: Caller<'_, ()>, params: &[Val], results: &mut [Val]) -> Result<(), wasmi::Error> {
    let w = state();
    let Some(ctx_ptr) = w.ctxs.borrow().last().copied() else {
        return Err(wasmi::Error::new("host function called outside of a JS call"));
    };
    // SAFETY: pushed by the JS frame that entered wasm and still running below us.
    let ctx = unsafe { &mut *ctx_ptr };
    w.callers.borrow_mut().push(&mut caller as *mut Caller<'_, ()> as *mut ());
    let r = (|| -> JsResult<()> {
        refresh_memories(&w, ctx)?;
        let f = w.js_funcs.borrow()[idx].clone();
        let mut args = Vec::with_capacity(params.len());
        for p in params {
            args.push(val_to_js(&w, p, ctx)?);
        }
        let ret = f.call(&JsValue::undefined(), &args, ctx)?;
        match result_types.len() {
            0 => {}
            1 => results[0] = js_to_val(&w, &ret, result_types[0], ctx)?,
            n => {
                let obj = ret.as_object().ok_or_else(|| type_error("multi-value results must be iterable"))?;
                let len = obj.get(js_string!("length"), ctx)?.to_length(ctx)? as usize;
                if len != n {
                    return Err(type_error("multi-value results have the wrong length"));
                }
                for (i, t) in result_types.iter().enumerate() {
                    let v = obj.get(i, ctx)?;
                    results[i] = js_to_val(&w, &v, *t, ctx)?;
                }
            }
        }
        Ok(())
    })();
    w.callers.borrow_mut().pop();
    r.map_err(|e| {
        *w.pending.borrow_mut() = Some(e);
        wasmi::Error::new("JavaScript exception")
    })
}

fn host_function(w: &Wasm, js: JsObject, ty: FuncType) -> Func {
    let idx = {
        let mut f = w.js_funcs.borrow_mut();
        f.push(js);
        f.len() - 1
    };
    let result_types: Vec<ValType> = ty.results().to_vec();
    with_store(w, |s| Func::new(s, ty, move |caller, params, results| host_call(idx, &result_types, caller, params, results)))
}

// ---------------------------------------------------------------------------
// Memories
// ---------------------------------------------------------------------------

fn new_memory(w: &Wasm, ty: MemoryType, ctx: &mut Context) -> JsResult<Memory> {
    let min = usize::try_from(ty.minimum()).ok().and_then(|p| p.checked_mul(PAGE)).ok_or_else(|| range_error("WebAssembly.Memory(): could not allocate memory"))?;
    let max_bytes = ty.maximum().and_then(|m| usize::try_from(m).ok()).and_then(|p| p.checked_mul(PAGE)).unwrap_or(4 << 30);
    let floor = min.max(PAGE);
    let mut cap = max_bytes.min(4 << 30).max(floor);
    let ptr = loop {
        let layout = std::alloc::Layout::from_size_align(cap, ALIGN).map_err(|_| range_error("WebAssembly.Memory(): could not allocate memory"))?;
        // Not `alloc_zeroed`: for this alignment the system allocator would memset (commit) the
        // whole reservation. wasmi zero-fills the initial size and every growth itself; the
        // untouched rest of a large reservation stays uncommitted virtual memory.
        // SAFETY: non-zero size; freed at teardown with the same layout.
        let p = unsafe { std::alloc::alloc(layout) };
        if !p.is_null() {
            break p;
        }
        if cap <= floor {
            return Err(range_error("WebAssembly.Memory(): could not allocate memory"));
        }
        cap = (cap / 2).max(floor);
    };
    // SAFETY: `ptr` is a fresh allocation of `cap` bytes that stays alive as long as the memory
    // slot roots its ArrayBuffer (until teardown, after the store stops using it).
    let slice: &'static mut [u8] = unsafe { std::slice::from_raw_parts_mut(ptr, cap) };
    let mem = match with_store(w, |s| Memory::new_static(s, ty, slice)) {
        Ok(m) => m,
        Err(e) => {
            // SAFETY: allocated above with this layout and not handed out.
            unsafe { std::alloc::dealloc(ptr, std::alloc::Layout::from_size_align_unchecked(cap, ALIGN)) };
            return Err(range_error(&format!("WebAssembly.Memory(): {e}")));
        }
    };
    // SAFETY: the allocation above lives until teardown (wasmi zeroed the first `min` bytes).
    let buffer = unsafe { JsArrayBuffer::from_external(ptr, min, ctx)? };
    w.memories.borrow_mut().insert(key(&mem), MemSlot { mem, buffer, len: min, ptr, cap });
    Ok(mem)
}

fn range_error(msg: &str) -> JsError {
    crate::js::JsNativeError::range().with_message(msg.to_string()).into()
}

/// Detach the buffers of memories wasm grew, as V8 does at growth: JS that caches a view of
/// `memory.buffer` (wasm-bindgen's `cachedUint8ArrayMemory0`) sees `byteLength === 0` when the
/// wasm call that grew it returns, or when it calls back into JS.
fn refresh_memories(w: &Wasm, ctx: &mut Context) -> JsResult<()> {
    let memories: Vec<Memory> = w.memories.borrow().values().map(|slot| slot.mem).collect();
    for mem in memories {
        memory_buffer(w, mem, ctx)?;
    }
    Ok(())
}

/// The memory's current ArrayBuffer (a new one after growth; the old one is detached).
fn memory_buffer(w: &Wasm, mem: Memory, ctx: &mut Context) -> JsResult<JsValue> {
    let size = with_store(w, |s| mem.data_size(s));
    let mut memories = w.memories.borrow_mut();
    let slot = memories.get_mut(&key(&mem)).ok_or_else(|| type_error("unknown WebAssembly memory"))?;
    if size != slot.len {
        if slot.buffer.is_detached() {
            return Err(type_error("the WebAssembly.Memory buffer was detached by JavaScript"));
        }
        slot.buffer.detach_in_place();
        // SAFETY: the allocation's capacity is the memory's reservation and wasmi grew within it
        // (zero-filled); `size` never exceeds it.
        slot.buffer = unsafe { JsArrayBuffer::from_external(slot.ptr, size, ctx)? };
        slot.len = size;
    }
    Ok(slot.buffer.clone().into())
}

// ---------------------------------------------------------------------------
// Module bytes: turn defined memories into imports
// ---------------------------------------------------------------------------

fn read_leb(b: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0;
    loop {
        let byte = *b.get(*pos)?;
        *pos += 1;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

fn write_leb(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn write_name(s: &str, out: &mut Vec<u8>) {
    write_leb(s.len() as u64, out);
    out.extend_from_slice(s.as_bytes());
}

/// Rewrite `wasm` so that defined memories become imports from [`SYNTHETIC_MODULE`]. Returns
/// `None` when the module defines no memory (or cannot be parsed; wasmi reports the error).
fn rewrite_memories(wasm: &[u8]) -> Option<Vec<u8>> {
    if wasm.len() < 8 || &wasm[..4] != b"\0asm" {
        return None;
    }
    let mut sections: Vec<(u8, &[u8])> = Vec::new();
    let mut pos = 8;
    while pos < wasm.len() {
        let id = wasm[pos];
        pos += 1;
        let size = read_leb(wasm, &mut pos)? as usize;
        let payload = wasm.get(pos..pos.checked_add(size)?)?;
        sections.push((id, payload));
        pos += size;
    }
    let (_, mem_payload) = *sections.iter().find(|(id, _)| *id == 5)?;
    let mut p = 0;
    let count = read_leb(mem_payload, &mut p)?;
    if count == 0 {
        return None;
    }
    let mut memtypes = Vec::new();
    for _ in 0..count {
        let start = p;
        let flags = read_leb(mem_payload, &mut p)?;
        read_leb(mem_payload, &mut p)?;
        if flags & 1 != 0 {
            read_leb(mem_payload, &mut p)?;
        }
        if flags & 8 != 0 {
            read_leb(mem_payload, &mut p)?;
        }
        memtypes.push(&mem_payload[start..p]);
    }
    // New import section payload.
    let mut imports = Vec::new();
    let (old_count, old_entries) = match sections.iter().find(|(id, _)| *id == 2) {
        Some((_, payload)) => {
            let mut q = 0;
            let n = read_leb(payload, &mut q)?;
            (n, &payload[q..])
        }
        None => (0, &[][..]),
    };
    write_leb(old_count + memtypes.len() as u64, &mut imports);
    imports.extend_from_slice(old_entries);
    for (i, ty) in memtypes.iter().enumerate() {
        write_name(SYNTHETIC_MODULE, &mut imports);
        write_name(&format!("memory{i}"), &mut imports);
        imports.push(0x02);
        imports.extend_from_slice(ty);
    }
    let mut out = wasm[..8].to_vec();
    let push = |out: &mut Vec<u8>, id: u8, payload: &[u8]| {
        out.push(id);
        write_leb(payload.len() as u64, out);
        out.extend_from_slice(payload);
    };
    let mut wrote_imports = false;
    for (id, payload) in &sections {
        match *id {
            2 => {
                push(&mut out, 2, &imports);
                wrote_imports = true;
            }
            5 => {}
            0 | 1 => push(&mut out, *id, payload),
            _ => {
                if !wrote_imports {
                    push(&mut out, 2, &imports);
                    wrote_imports = true;
                }
                push(&mut out, *id, payload);
            }
        }
    }
    if !wrote_imports {
        push(&mut out, 2, &imports);
    }
    Some(out)
}

/// Export names in declaration order (wasmi reports them sorted).
fn export_order(wasm: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 8;
    while pos < wasm.len() {
        let id = wasm[pos];
        pos += 1;
        let Some(size) = read_leb(wasm, &mut pos) else { break };
        let end = pos + size as usize;
        if end > wasm.len() {
            break;
        }
        if id == 7 {
            let mut q = pos;
            let Some(count) = read_leb(wasm, &mut q) else { break };
            for _ in 0..count {
                let Some(n) = read_leb(wasm, &mut q) else { break };
                let name_end = q + n as usize;
                if name_end > end {
                    break;
                }
                out.push(String::from_utf8_lossy(&wasm[q..name_end]).into_owned());
                q = name_end + 1;
                if read_leb(wasm, &mut q).is_none() {
                    break;
                }
            }
        }
        pos = end;
    }
    out
}

/// Custom sections of a module: `(name, data)`.
fn custom_sections(wasm: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 8;
    while pos < wasm.len() {
        let id = wasm[pos];
        pos += 1;
        let Some(size) = read_leb(wasm, &mut pos) else { break };
        let end = pos + size as usize;
        if end > wasm.len() {
            break;
        }
        if id == 0 {
            let mut q = pos;
            if let Some(n) = read_leb(wasm, &mut q) {
                let name_end = q + n as usize;
                if name_end <= end {
                    out.push((String::from_utf8_lossy(&wasm[q..name_end]).into_owned(), wasm[name_end..end].to_vec()));
                }
            }
        }
        pos = end;
    }
    out
}

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

struct WasmModule {
    module: Module,
    custom: Vec<(String, Vec<u8>)>,
    exports: Vec<String>,
}

fn kind_of(ty: &ExternType) -> &'static str {
    match ty {
        ExternType::Func(_) => "function",
        ExternType::Table(_) => "table",
        ExternType::Memory(_) => "memory",
        ExternType::Global(_) => "global",
    }
}

fn val_type_name(t: ValType) -> &'static str {
    match t {
        ValType::I32 => "i32",
        ValType::I64 => "i64",
        ValType::F32 => "f32",
        ValType::F64 => "f64",
        ValType::V128 => "v128",
        ValType::FuncRef => "anyfunc",
        ValType::ExternRef => "externref",
    }
}

fn parse_val_type(s: &str) -> Option<ValType> {
    Some(match s {
        "i32" => ValType::I32,
        "i64" => ValType::I64,
        "f32" => ValType::F32,
        "f64" => ValType::F64,
        "anyfunc" | "funcref" => ValType::FuncRef,
        "externref" => ValType::ExternRef,
        _ => return None,
    })
}

/// `ops.wasmCompile(bytes) -> module`
fn op_wasm_compile(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let bytes = bytes_arg(args.get_or_undefined(0), ctx)?;
    let w = state();
    let rewritten = rewrite_memories(&bytes);
    let module = match Module::new(&w.engine, rewritten.as_deref().unwrap_or(&bytes)) {
        Ok(m) => m,
        Err(e) => return Err(wasm_error(ctx, "CompileError", &format!("WebAssembly.Module(): {e}"))),
    };
    let mut exports = export_order(&bytes);
    // Fall back to wasmi's list if the export section could not be read.
    if exports.len() != module.exports().count() {
        exports = module.exports().map(|e| e.name().to_string()).collect();
    }
    Ok(native_object(WasmModule { module, custom: custom_sections(&bytes), exports }))
}

/// `ops.wasmValidate(bytes) -> boolean`
fn op_wasm_validate(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let bytes = bytes_arg(args.get_or_undefined(0), ctx)?;
    Ok(JsValue::from(Module::validate(&state().engine, &bytes).is_ok()))
}

/// `ops.wasmModuleInfo(module, "imports"|"exports") -> [{ module?, name, kind }]`
fn op_wasm_module_info(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let which = arg_string(args, 1, ctx)?;
    let entries: Vec<(Option<String>, String, &'static str)> = with_native::<WasmModule, _>(args.get_or_undefined(0), |m| {
        if which == "imports" {
            m.module
                .imports()
                .filter(|i| i.module() != SYNTHETIC_MODULE)
                .map(|i| (Some(i.module().to_string()), i.name().to_string(), kind_of(i.ty())))
                .collect()
        } else {
            m.exports.iter().filter_map(|name| m.module.get_export(name).map(|ty| (None, name.clone(), kind_of(&ty)))).collect()
        }
    })?;
    let mut items = Vec::new();
    for (module, name, kind) in entries {
        let mut fields = Vec::new();
        if let Some(m) = module {
            fields.push(("module", js_str(&m)));
        }
        fields.push(("name", js_str(&name)));
        fields.push(("kind", js_str(kind)));
        items.push(object(ctx, fields)?);
    }
    Ok(super::array(ctx, items))
}

/// `ops.wasmCustomSections(module, name) -> ArrayBuffer[]`
fn op_wasm_custom_sections(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_string(args, 1, ctx)?;
    let sections: Vec<Vec<u8>> = with_native::<WasmModule, _>(args.get_or_undefined(0), |m| m.custom.iter().filter(|(n, _)| *n == name).map(|(_, d)| d.clone()).collect())?;
    let mut items = Vec::new();
    for s in sections {
        items.push(JsArrayBuffer::from_byte_block(s, ctx)?.into());
    }
    Ok(super::array(ctx, items))
}

fn extern_from_handle(v: &JsValue) -> Option<Extern> {
    if let Ok(m) = with_native::<Memory, _>(v, |m| *m) {
        return Some(Extern::Memory(m));
    }
    if let Ok(t) = with_native::<Table, _>(v, |t| *t) {
        return Some(Extern::Table(t));
    }
    if let Ok(g) = with_native::<Global, _>(v, |g| *g) {
        return Some(Extern::Global(g));
    }
    None
}

fn extern_to_js(w: &Wasm, name: &str, e: Extern, ctx: &mut Context) -> JsResult<(JsValue, &'static str)> {
    Ok(match e {
        Extern::Func(f) => (export_function(w, f, Some(name), ctx)?.into(), "function"),
        Extern::Memory(m) => (native_object(m), "memory"),
        Extern::Table(t) => (native_object(t), "table"),
        Extern::Global(g) => (native_object(g), "global"),
    })
}

/// `ops.wasmInstantiate(module, values) -> [[name, kind, value]]` where `values` holds the JS
/// value for each (non-synthetic) import in order: functions, or memory/table/global handles, or
/// numbers for immutable global imports.
fn op_wasm_instantiate(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let w = state();
    let (module, names) = with_native::<WasmModule, _>(args.get_or_undefined(0), |m| (m.module.clone(), m.exports.clone()))?;
    let values = args.get_or_undefined(1).as_object().ok_or_else(|| type_error("imports must be an array"))?;
    let imports: Vec<(String, String, ExternType)> = module.imports().map(|i| (i.module().to_string(), i.name().to_string(), i.ty().clone())).collect();
    let mut externs = Vec::with_capacity(imports.len());
    let mut js_index = 0u32;
    for (index, (module_name, name, ty)) in imports.iter().enumerate() {
        if module_name == SYNTHETIC_MODULE {
            let ExternType::Memory(mt) = ty else { unreachable!() };
            externs.push(Extern::Memory(new_memory(&w, *mt, ctx)?));
            continue;
        }
        let value = values.get(js_index, ctx)?;
        js_index += 1;
        let link = |ctx: &mut Context, what: &str| {
            wasm_error(ctx, "LinkError", &format!("WebAssembly.Instance(): Import #{index} \"{module_name}\" \"{name}\": {what}"))
        };
        let ext = match ty {
            ExternType::Func(ft) => {
                let Some(obj) = value.as_object().filter(|o| o.is_callable()) else {
                    return Err(link(ctx, "function import requires a callable"));
                };
                match wasm_func_of(&w, &value, ctx)? {
                    Some(f) => {
                        let actual = with_store(&w, |s| f.ty(s));
                        if actual != *ft {
                            return Err(link(ctx, "imported function does not match the expected type"));
                        }
                        Extern::Func(f)
                    }
                    None => Extern::Func(host_function(&w, obj, ft.clone())),
                }
            }
            ExternType::Memory(_) => match extern_from_handle(&value) {
                Some(e @ Extern::Memory(_)) => e,
                _ => return Err(link(ctx, "memory import must be a WebAssembly.Memory object")),
            },
            ExternType::Table(_) => match extern_from_handle(&value) {
                Some(e @ Extern::Table(_)) => e,
                _ => return Err(link(ctx, "table import requires a WebAssembly.Table")),
            },
            ExternType::Global(gt) => match extern_from_handle(&value) {
                Some(e @ Extern::Global(_)) => e,
                _ => {
                    if gt.mutability() == Mutability::Var {
                        return Err(link(ctx, "imported mutable global must be a WebAssembly.Global object"));
                    }
                    let is_num = value.is_number();
                    let is_big = value.is_bigint();
                    let ok = match gt.content() {
                        ValType::I64 => is_big,
                        ValType::I32 | ValType::F32 | ValType::F64 => is_num,
                        _ => true,
                    };
                    if !ok {
                        return Err(link(ctx, "global import must be a number, valid Wasm reference, or WebAssembly.Global object"));
                    }
                    let v = js_to_val(&w, &value, gt.content(), ctx)?;
                    Extern::Global(with_store(&w, |s| Global::new(s, v, Mutability::Const)))
                }
            },
        };
        externs.push(ext);
    }
    w.ctxs.borrow_mut().push(ctx as *mut Context);
    let r = with_store(&w, |s| wasmi::Instance::new(s, &module, &externs));
    w.ctxs.borrow_mut().pop();
    let instance = match r {
        Ok(i) => i,
        Err(e) => {
            if let Some(js) = w.pending.borrow_mut().take() {
                return Err(js);
            }
            if e.as_trap_code().is_some() {
                return Err(trap_error(ctx, &e));
            }
            return Err(wasm_error(ctx, "LinkError", &format!("WebAssembly.Instance(): {e}")));
        }
    };
    let mut items = Vec::new();
    for name in names {
        let Some(ext) = with_store(&w, |s| instance.get_export(s, &name)) else { continue };
        let (value, kind) = extern_to_js(&w, &name, ext, ctx)?;
        items.push(super::array(ctx, vec![js_str(&name), js_str(kind), value]));
    }
    Ok(super::array(ctx, items))
}

/// `ops.wasmHandleKey(handle) -> string` (identity of a memory/table/global).
fn op_wasm_handle_key(_: &JsValue, args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let v = args.get_or_undefined(0);
    let k = match extern_from_handle(v) {
        Some(Extern::Memory(m)) => key(&m),
        Some(Extern::Table(t)) => key(&t),
        Some(Extern::Global(g)) => key(&g),
        _ => return Err(type_error("invalid WebAssembly handle")),
    };
    Ok(js_str(&k))
}

fn opt_u32(v: &JsValue, ctx: &mut Context) -> JsResult<Option<u32>> {
    if v.is_undefined() { Ok(None) } else { Ok(Some(v.to_number(ctx)? as u32)) }
}

/// `ops.wasmMemoryNew(initial, maximum?) -> handle`
fn op_wasm_memory_new(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let initial = arg_f64(args, 0, ctx)? as u32;
    let maximum = opt_u32(args.get_or_undefined(1), ctx)?;
    let w = state();
    let ty = MemoryType::new(initial, maximum);
    Ok(native_object(new_memory(&w, ty, ctx)?))
}

/// `ops.wasmMemory(handle, op, arg)`: `buffer` -> ArrayBuffer, `grow` (arg = pages) -> old pages.
fn op_wasm_memory(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let mem = with_native::<Memory, _>(args.get_or_undefined(0), |m| *m)?;
    let op = arg_string(args, 1, ctx)?;
    let w = state();
    match op.as_str() {
        "buffer" => memory_buffer(&w, mem, ctx),
        "grow" => {
            let delta = arg_f64(args, 2, ctx)? as u64;
            match with_store(&w, |s| mem.grow(s, delta)) {
                Ok(old) => {
                    memory_buffer(&w, mem, ctx)?;
                    Ok(JsValue::from(old as f64))
                }
                Err(_) => Err(range_error("WebAssembly.Memory.grow(): Maximum memory size exceeded")),
            }
        }
        _ => {
            let ty = with_store(&w, |s| mem.ty(s));
            let size = with_store(&w, |s| mem.size(s));
            object(
                ctx,
                vec![
                    ("minimum", JsValue::from(size as f64)),
                    ("maximum", ty.maximum().map(|m| JsValue::from(m as f64)).unwrap_or_default()),
                ],
            )
        }
    }
}

/// `ops.wasmTableNew(element, initial, maximum?, init) -> handle`
fn op_wasm_table_new(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let element = arg_string(args, 0, ctx)?;
    let initial = arg_f64(args, 1, ctx)? as u32;
    let maximum = opt_u32(args.get_or_undefined(2), ctx)?;
    let ty = match element.as_str() {
        "anyfunc" | "funcref" => RefType::Func,
        "externref" => RefType::Extern,
        _ => return Err(type_error("WebAssembly.Table(): Descriptor property 'element' must be a WebAssembly reference type")),
    };
    let w = state();
    let init_v = args.get_or_undefined(3);
    // Like V8: funcref tables start null, externref tables start undefined.
    let init = match (ty, init_v.is_undefined()) {
        (RefType::Func, true) => Ref::Func(Nullable::Null),
        _ => js_to_ref(&w, init_v, ty, ctx)?,
    };
    let table = with_store(&w, |s| Table::new(s, TableType::new(ty, initial, maximum), init)).map_err(|e| range_error(&format!("WebAssembly.Table(): {e}")))?;
    Ok(native_object(table))
}

/// `ops.wasmTable(handle, op, a, b)`: `get`, `set`, `grow`, `length`, `element`.
fn op_wasm_table(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let table = with_native::<Table, _>(args.get_or_undefined(0), |t| *t)?;
    let op = arg_string(args, 1, ctx)?;
    let w = state();
    let ty = with_store(&w, |s| table.ty(s));
    match op.as_str() {
        "length" => Ok(JsValue::from(with_store(&w, |s| table.size(s)) as f64)),
        "element" => Ok(js_str(if ty.element() == RefType::Func { "anyfunc" } else { "externref" })),
        "get" => {
            let i = arg_f64(args, 2, ctx)? as u64;
            match with_store(&w, |s| table.get(s, i)) {
                Some(r) => ref_to_js(&w, r, ctx),
                None => Err(range_error("WebAssembly.Table.get(): invalid address")),
            }
        }
        "set" => {
            let i = arg_f64(args, 2, ctx)? as u64;
            let v = js_to_ref(&w, args.get_or_undefined(3), ty.element(), ctx)?;
            with_store(&w, |s| table.set(s, i, v)).map_err(|_| range_error("WebAssembly.Table.set(): invalid address"))?;
            Ok(JsValue::undefined())
        }
        _ => {
            let delta = arg_f64(args, 2, ctx)? as u64;
            let init_v = args.get_or_undefined(3);
            let init = if init_v.is_undefined() {
                match ty.element() {
                    RefType::Func => Ref::Func(Nullable::Null),
                    RefType::Extern => Ref::Extern(Nullable::Null),
                }
            } else {
                js_to_ref(&w, init_v, ty.element(), ctx)?
            };
            match with_store(&w, |s| table.grow(s, delta, init)) {
                Ok(old) => Ok(JsValue::from(old as f64)),
                Err(_) => Err(range_error("WebAssembly.Table.grow(): failed to grow table")),
            }
        }
    }
}

/// `ops.wasmGlobalNew(type, mutable, value) -> handle`
fn op_wasm_global_new(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ty_name = arg_string(args, 0, ctx)?;
    let Some(ty) = parse_val_type(&ty_name) else {
        return Err(type_error("WebAssembly.Global(): Descriptor property 'value' must be a WebAssembly type"));
    };
    let mutable = args.get_or_undefined(1).to_boolean();
    let w = state();
    let value = args.get_or_undefined(2);
    let v = if value.is_undefined() { Val::default_for_ty(ty) } else { js_to_val(&w, value, ty, ctx)? };
    let g = with_store(&w, |s| Global::new(s, v, if mutable { Mutability::Var } else { Mutability::Const }));
    Ok(native_object(g))
}

/// `ops.wasmGlobal(handle, op, value)`: `get`, `set`, `type` -> { value, mutable }.
fn op_wasm_global(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let g = with_native::<Global, _>(args.get_or_undefined(0), |g| *g)?;
    let op = arg_string(args, 1, ctx)?;
    let w = state();
    let ty = with_store(&w, |s| g.ty(s));
    match op.as_str() {
        "get" => {
            let v = with_store(&w, |s| g.get(s));
            val_to_js(&w, &v, ctx)
        }
        "set" => {
            if ty.mutability() != Mutability::Var {
                return Err(type_error("Can't set the value of an immutable global."));
            }
            let v = js_to_val(&w, args.get_or_undefined(2), ty.content(), ctx)?;
            with_store(&w, |s| g.set(s, v)).map_err(|e| type_error(e.to_string()))?;
            Ok(JsValue::undefined())
        }
        _ => object(ctx, vec![("value", js_str(val_type_name(ty.content()))), ("mutable", JsValue::from(ty.mutability() == Mutability::Var))]),
    }
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("wasmCompile", 1, op_wasm_compile),
    ("wasmValidate", 1, op_wasm_validate),
    ("wasmModuleInfo", 2, op_wasm_module_info),
    ("wasmCustomSections", 2, op_wasm_custom_sections),
    ("wasmInstantiate", 2, op_wasm_instantiate),
    ("wasmHandleKey", 1, op_wasm_handle_key),
    ("wasmMemoryNew", 2, op_wasm_memory_new),
    ("wasmMemory", 3, op_wasm_memory),
    ("wasmTableNew", 4, op_wasm_table_new),
    ("wasmTable", 4, op_wasm_table),
    ("wasmGlobalNew", 3, op_wasm_global_new),
    ("wasmGlobal", 3, op_wasm_global),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_defined_memory_into_an_import() {
        // (module (memory (export "mem") 1 2) (func (export "f") (result i32) i32.const 7))
        let wasm: Vec<u8> = vec![
            0, 97, 115, 109, 1, 0, 0, 0, // header
            1, 5, 1, 96, 0, 1, 127, // type: () -> i32
            3, 2, 1, 0, // function
            5, 4, 1, 1, 1, 2, // memory: min 1 max 2
            7, 11, 2, 3, 109, 101, 109, 2, 0, 1, 102, 0, 0, // exports
            10, 6, 1, 4, 0, 65, 7, 11, // code
        ];
        let engine = wasmi::Engine::default();
        Module::new(&engine, &wasm).unwrap();
        let out = rewrite_memories(&wasm).unwrap();
        let m = Module::new(&engine, &out).unwrap();
        let imports: Vec<_> = m.imports().map(|i| (i.module().to_string(), i.name().to_string())).collect();
        assert_eq!(imports, vec![(SYNTHETIC_MODULE.to_string(), "memory0".to_string())]);
        let ExternType::Memory(mt) = m.imports().next().unwrap().ty().clone() else { panic!() };
        assert_eq!((mt.minimum(), mt.maximum()), (1, Some(2)));
        assert!(m.exports().any(|e| e.name() == "mem"));
    }
}
