//! `node:vm`: every context is a new QuickJS context (realm: its own intrinsics `Object`, `Array`, ...).
//!
//! A contextified object behaves like V8's: the realm's global object is a Proxy (built by
//! `js/node/vm.js` in the outer realm) that resolves names on the contextified object first and
//! on the realm's builtins next, and sends assignments and declarations to the contextified
//! object. The Proxy becomes the realm's global object (`JS_SetGlobalObject`, vendored QuickJS patch).

use crate::js::rquickjs::qjs;
use crate::js::{Context, JsArgs, JsData, JsObject, JsResult, JsValue, Realm};

use crate::engine::{self, OpFn, arg_opt_string, arg_string, state};

/// A realm referenced by a JS object (realms live as long as the engine).
struct VmRealm {
    realm: Realm,
}

impl JsData for VmRealm {}

const REALM_GLUE: &str = r#"
// Per-realm setup of a vm context: Node-style Error.stack, Error.captureStackTrace and
// ArrayBuffer.prototype.transfer (as in the main realm's prelude).
const desc = Object.getOwnPropertyDescriptor(Error.prototype, "stack");
if (desc && typeof desc.get === "function") {
  const originalGet = desc.get;
  const header = (err) => {
    let name, message;
    try {
      name = err.name === undefined ? "Error" : String(err.name);
      message = err.message === undefined ? "" : String(err.message);
    } catch {
      name = "Error";
      message = "";
    }
    if (!name) return message;
    if (!message) return name;
    return `${name}: ${message}`;
  };
  Object.defineProperty(Error.prototype, "stack", {
    configurable: true,
    enumerable: false,
    get() {
      let frames = "";
      try {
        frames = originalGet.call(this) || "";
      } catch {}
      frames = frames.replace(/\n?\s*at\s+\(native\)\s*$/g, "").replace(/\s+$/, "");
      if (frames) frames = ops.remapStack(frames);
      return frames ? `${header(this)}\n${frames}` : header(this);
    },
    set(value) {
      Object.defineProperty(this, "stack", { value, writable: true, configurable: true, enumerable: false });
    },
  });
}
Error.stackTraceLimit = 10;
Error.captureStackTrace = function captureStackTrace(target) {
  if (target === null || (typeof target !== "object" && typeof target !== "function")) throw new TypeError("Invalid argument");
  const frames = new Error().stack.split("\n").slice(2).join("\n");
  let head;
  try {
    head = target.message ? `${target.name ?? "Error"}: ${target.message}` : String(target.name ?? "Error");
  } catch {
    head = "Error";
  }
  Object.defineProperty(target, "stack", { value: frames ? `${head}\n${frames}` : head, writable: true, configurable: true, enumerable: false });
};
if (typeof ArrayBuffer.prototype.transfer !== "function") {
  for (const [name, fixed] of [["transfer", false], ["transferToFixedLength", true]]) {
    Object.defineProperty(ArrayBuffer.prototype, name, {
      value: { [name](n) { return ops.arrayBufferTransfer(this, n, !fixed && this.resizable ? this.maxByteLength : undefined); } }[name],
      writable: true, configurable: true, enumerable: false,
    });
  }
  Object.defineProperty(ArrayBuffer.prototype, "detached", { get: function () { return ops.arrayBufferDetached(this); }, configurable: true, enumerable: false });
}
"#;

fn realm_of(v: &JsValue) -> Option<Realm> {
    let obj = v.as_object()?;
    let data = obj.downcast_ref::<VmRealm>()?;
    Some(data.realm)
}

/// Run `f` with `realm` as the current realm.
fn in_realm<R>(_ctx: &mut Context, realm: &Realm, f: impl FnOnce(&mut Context) -> R) -> R {
    let mut ctx = Context::for_realm(realm);
    f(&mut ctx)
}

/// `ops.vmCreateRealm(global?, target?) -> realm handle`: a new realm whose global object is
/// `global` (a Proxy whose target is `target`), or an ordinary global object.
fn op_vm_create_realm(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let realm = ctx.create_realm()?;
    if let (Some(global), Some(target)) = (args.get_or_undefined(0).as_object(), args.get_or_undefined(1).as_object()) {
        in_realm(ctx, &realm, |ctx| install_global(ctx, &global, &target))?;
    }
    let ops = state(ctx).ops.borrow().clone();
    in_realm(ctx, &realm, |ctx| -> JsResult<()> {
        let wrapped = format!("(function (ops) {{\"use strict\";\n{REALM_GLUE}\n}})");
        let f = ctx.eval_with_flags(wrapped.as_bytes(), "pi-jsrt:vm-realm.js", qjs::JS_EVAL_TYPE_GLOBAL as i32)?;
        if let (Some(f), Some(ops)) = (f.as_function(), ops) {
            f.call(&JsValue::undefined(), &[ops.into()], ctx)?;
        }
        Ok(())
    })?;
    let proto = ctx.intrinsics().constructors().object().prototype();
    Ok(JsObject::from_proto_and_data(Some(proto), VmRealm { realm }).into())
}

/// Make `global` (a Proxy over `target`) the realm's global object: the realm's builtins move to
/// `target`, whose prototype becomes the realm's `Object.prototype`.
fn install_global(ctx: &mut Context, global: &JsObject, target: &JsObject) -> JsResult<()> {
    let original = ctx.global_object();
    for key in original.own_property_keys(ctx)? {
        if let Some(desc) = original.get_own_property(key.clone(), ctx)? {
            target.define_property_or_throw(key, desc, ctx)?;
        }
    }
    target.define_property_or_throw(
        "globalThis",
        crate::js::PropertyDescriptor::builder().value(global.clone()).writable(true).enumerable(false).configurable(true),
        ctx,
    )?;
    target.set_prototype(Some(ctx.intrinsics().constructors().object().prototype()));
    ctx.set_global_object(global);
    Ok(())
}

/// `ops.vmGlobal(realm) -> globalThis of the realm`
fn op_vm_global(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let realm = realm_of(args.get_or_undefined(0)).ok_or_else(|| engine::type_error("invalid vm context"))?;
    Ok(in_realm(ctx, &realm, |ctx| ctx.global_object()).into())
}

/// Shift positions like V8's `lineOffset`/`columnOffset` (QuickJS sources start at 1:1).
fn with_offsets(code: &str, line_offset: i64, column_offset: i64) -> String {
    let mut out = String::new();
    for _ in 0..line_offset.max(0) {
        out.push('\n');
    }
    for _ in 0..column_offset.max(0) {
        out.push(' ');
    }
    out.push_str(code);
    out
}

/// `ops.vmCompile(code, filename)`: parse only (throws the SyntaxError), like `new vm.Script`.
fn op_vm_compile(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let code = arg_string(args, 0, ctx)?;
    let filename = arg_opt_string(args, 1, ctx)?.unwrap_or_else(|| "evalmachine.<anonymous>".into());
    let flags = (qjs::JS_EVAL_TYPE_GLOBAL | qjs::JS_EVAL_FLAG_COMPILE_ONLY) as i32;
    drop(ctx.eval_with_flags(code.as_bytes(), &filename, flags)?);
    Ok(JsValue::undefined())
}

/// `ops.vmRun(realm | undefined, code, filename, lineOffset, columnOffset)`: evaluate a script in
/// the realm (the current one when `undefined`) and return its completion value.
fn op_vm_run(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let realm = realm_of(args.get_or_undefined(0));
    let code = arg_string(args, 1, ctx)?;
    let filename = arg_opt_string(args, 2, ctx)?.unwrap_or_else(|| "evalmachine.<anonymous>".into());
    let line = args.get_or_undefined(3).to_number(ctx).unwrap_or(0.0) as i64;
    let column = args.get_or_undefined(4).to_number(ctx).unwrap_or(0.0) as i64;
    let code = with_offsets(&code, line, column);
    let run = |ctx: &mut Context| -> JsResult<JsValue> { ctx.eval_with_flags(code.as_bytes(), &filename, qjs::JS_EVAL_TYPE_GLOBAL as i32) };
    match realm {
        Some(realm) => in_realm(ctx, &realm, run),
        None => run(ctx),
    }
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("vmCreateRealm", 2, op_vm_create_realm),
    ("vmGlobal", 1, op_vm_global),
    ("vmCompile", 2, op_vm_compile),
    ("vmRun", 5, op_vm_run),
];
