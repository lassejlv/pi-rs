//! Module resolution and loading: ES modules, CommonJS, JSON, builtins and
//! virtual modules, with Node-style interop between them.
//!
//! Resolution order for a specifier:
//! 1. embedder aliases (exact, or `alias/` prefix),
//! 2. embedder virtual modules (exact),
//! 3. builtins (`node:fs`, `fs`, `jsrt:host`, ...),
//! 4. `file:` URLs, absolute and relative paths (with extension / index probing,
//!    including `./x.js` -> `./x.ts` like TypeScript's resolver),
//! 5. packages in `node_modules` (walking up), honoring `package.json` `exports`
//!    (with `import`/`require`/`node`/`default` conditions and `*` patterns),
//!    `module` (for `import`) and `main`.
//!
//! File format: `.mjs/.mts` ESM, `.cjs/.cts` CommonJS, `.json` JSON; `.js` follows the
//! nearest `package.json` `type` and otherwise ES module syntax detection; `.ts`
//! uses syntax detection (like jiti). TypeScript is transpiled with oxc.
//! ES modules outside `node_modules` get jiti-compatible `__filename`, `__dirname`
//! and `require` bindings unless they declare those names themselves.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crate::js::rquickjs::qjs;
use crate::js::{
    Context, Engine, JsArgs, JsArray, JsError, JsNativeError, JsObject, JsPromise, JsResult, JsString, JsValue, PromiseState, Source, js_string,
};
use serde_json::Value;

use crate::engine::{self, arg_opt_string, arg_string, internal, js_str, state};
use crate::error::{self, Error};
use crate::runtime::{Shared, VirtualModule};
use crate::transpile::{self, SourceLang};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Resolved {
    Builtin(String),
    Virtual(String),
    File(PathBuf),
}

impl Resolved {
    /// The module name (QuickJS caches modules by name): files carry the cache generation
    /// (see [`Loader::clear_files`]).
    fn key_at(&self, generation: u32) -> String {
        match self {
            Resolved::Builtin(n) => format!("builtin:{n}"),
            Resolved::Virtual(s) => format!("virtual:{s}"),
            Resolved::File(p) if generation == 0 => p.display().to_string(),
            Resolved::File(p) => format!("{}{RELOAD_MARK}{generation}", p.display()),
        }
    }
}

/// Suffix of file module names after a cache clear, and of repeated `eval_module` names.
const RELOAD_MARK: &str = "?reload=";
const EVAL_MARK: &str = "?eval=";

/// The file path behind a module name (without the reload / eval suffix).
pub(crate) fn module_path(name: &str) -> &str {
    let end = [RELOAD_MARK, EVAL_MARK].iter().filter_map(|m| name.find(m)).min().unwrap_or(name.len());
    &name[..end]
}

/// A module to evaluate (see [`evaluate_module`]): its QuickJS module name.
pub(crate) struct Module(String);

/// Exports of a module made from a JS value (builtins, CommonJS, JSON), set when QuickJS
/// initializes the module.
struct Synthetic {
    value: JsValue,
    default: JsValue,
    names: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Import,
    Require,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Esm,
    Cjs,
    Json,
}

struct Prepared {
    code: String,
    format: Format,
}

/// The module loader installed in the QuickJS runtime.
pub(crate) struct Loader {
    shared: Arc<Shared>,
    /// Module name -> what it loads (filled when names are handed to QuickJS).
    names: RefCell<HashMap<String, Resolved>>,
    /// ES module source text for names created by [`module_from_source`].
    sources: RefCell<HashMap<String, String>>,
    /// Module names QuickJS has loaded.
    loaded: RefCell<std::collections::HashSet<String>>,
    synthetic: RefCell<HashMap<String, Synthetic>>,
    cjs: RefCell<HashMap<String, JsObject>>,
    values: RefCell<HashMap<String, JsValue>>,
    pkg_json: RefCell<HashMap<PathBuf, Option<Rc<Value>>>>,
    generation: Cell<u32>,
    evals: Cell<u32>,
}

impl Loader {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Loader {
            shared,
            names: RefCell::default(),
            sources: RefCell::default(),
            loaded: RefCell::default(),
            synthetic: RefCell::default(),
            cjs: RefCell::default(),
            values: RefCell::default(),
            pkg_json: RefCell::default(),
            generation: Cell::new(0),
            evals: Cell::new(0),
        }
    }

    fn key(&self, resolved: &Resolved) -> String {
        resolved.key_at(self.generation.get())
    }

    /// The module name for `resolved`, registered for the QuickJS loader callback.
    fn module_name(&self, resolved: &Resolved) -> String {
        let name = self.key(resolved);
        self.names.borrow_mut().entry(name.clone()).or_insert_with(|| resolved.clone());
        name
    }

    /// jiti-style `__filename`/`__dirname`/`require` for ES modules, unless disabled
    /// ([`crate::RuntimeOptions::esm_cjs_globals`]).
    fn cjs_globals(&self, code: &str, declared: &[&str]) -> String {
        if self.shared.options.esm_cjs_globals { inject_cjs_globals(code, declared) } else { code.to_string() }
    }

    pub(crate) fn clear(&self) {
        self.synthetic.borrow_mut().clear();
        self.cjs.borrow_mut().clear();
        self.values.borrow_mut().clear();
    }

    /// Forget loaded files: QuickJS cannot unload modules, so later loads use new module names
    /// (a new generation), which re-reads and re-evaluates the files.
    fn clear_files(&self) {
        self.generation.set(self.generation.get() + 1);
        self.cjs.borrow_mut().retain(|k, _| k.starts_with("builtin:") || k.starts_with("virtual:"));
        self.values.borrow_mut().retain(|k, _| k.starts_with("builtin:") || k.starts_with("virtual:"));
        self.pkg_json.borrow_mut().clear();
    }

    // -----------------------------------------------------------------------
    // Resolution
    // -----------------------------------------------------------------------

    fn apply_alias(&self, spec: &str) -> String {
        let aliases = self.shared.aliases.read();
        if aliases.is_empty() {
            return spec.to_string();
        }
        let mut current = spec.to_string();
        for _ in 0..8 {
            if let Some(t) = aliases.get(&current) {
                current = t.clone();
                continue;
            }
            let mut best: Option<(&String, &String)> = None;
            for (from, to) in aliases.iter() {
                if current.len() > from.len()
                    && current.starts_with(from.as_str())
                    && current.as_bytes()[from.len()] == b'/'
                    && best.is_none_or(|(b, _)| from.len() > b.len())
                {
                    best = Some((from, to));
                }
            }
            match best {
                Some((from, to)) => current = format!("{to}{}", &current[from.len()..]),
                None => break,
            }
        }
        current
    }

    pub(crate) fn resolve(&self, ctx: &mut Context, spec: &str, base_dir: &Path, kind: Kind) -> Result<Resolved, Error> {
        let resolved = self.resolve_spec(ctx, spec, base_dir, kind)?;
        // An alias keyed by an absolute file path redirects every import resolving to that file.
        if let Resolved::File(path) = &resolved {
            let target = self.shared.aliases.read().get(&*path.to_string_lossy()).cloned();
            if let Some(target) = target {
                return self.resolve_spec(ctx, &target, base_dir, kind);
            }
        }
        Ok(resolved)
    }

    fn resolve_spec(&self, ctx: &mut Context, spec: &str, base_dir: &Path, kind: Kind) -> Result<Resolved, Error> {
        let spec = self.apply_alias(spec);
        if self.shared.virtual_modules.read().contains_key(&spec) {
            return Ok(Resolved::Virtual(spec));
        }
        if let Some(name) = builtin_name(ctx, &spec) {
            return Ok(Resolved::Builtin(name));
        }
        let not_found = || Error::ModuleNotFound {
            specifier: spec.clone(),
            referrer: base_dir.display().to_string(),
        };
        if let Some(rest) = spec.strip_prefix("file://") {
            let path = url::Url::parse(&format!("file://{rest}"))
                .ok()
                .and_then(|u| u.to_file_path().ok())
                .unwrap_or_else(|| PathBuf::from(rest));
            return self.resolve_path(&path, kind).map(Resolved::File).ok_or_else(not_found);
        }
        if spec.starts_with('/') {
            return self
                .resolve_path(&normalize(Path::new(&spec)), kind)
                .map(Resolved::File)
                .ok_or_else(not_found);
        }
        if spec == "." || spec == ".." || spec.starts_with("./") || spec.starts_with("../") {
            let p = normalize(&base_dir.join(&spec));
            return self.resolve_path(&p, kind).map(Resolved::File).ok_or_else(not_found);
        }
        if spec.starts_with('#') {
            return self.resolve_imports(&spec, base_dir, kind).map(Resolved::File).ok_or_else(not_found);
        }
        self.resolve_package(&spec, base_dir, kind).map(Resolved::File).ok_or_else(not_found)
    }

    /// package.json `imports` (`#internal` specifiers) of the nearest package.
    fn resolve_imports(&self, spec: &str, base_dir: &Path, kind: Kind) -> Option<PathBuf> {
        let mut dir = Some(base_dir);
        while let Some(d) = dir {
            if let Some(pkg) = self.package_json(d) {
                let imports = pkg.get("imports")?;
                let conditions: &[&str] = match kind {
                    Kind::Import => &["import", "module", "node", "default"],
                    Kind::Require => &["require", "node", "default"],
                };
                let target = resolve_exports(imports, spec, conditions)?;
                if target.starts_with("./") || target.starts_with("../") {
                    return self.resolve_file_or_index(&normalize(&d.join(target)));
                }
                // A bare target refers to a dependency.
                return self.resolve_package(&target, d, kind);
            }
            dir = d.parent();
        }
        None
    }

    fn resolve_path(&self, p: &Path, kind: Kind) -> Option<PathBuf> {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
        let s = p.to_string_lossy();
        // TypeScript-style: "./x.js" may refer to "./x.ts".
        for (from, tos) in [
            (".js", &[".ts", ".tsx"][..]),
            (".mjs", &[".mts"][..]),
            (".cjs", &[".cts"][..]),
            (".jsx", &[".tsx"][..]),
        ] {
            if let Some(stem) = s.strip_suffix(from) {
                for to in tos {
                    let c = PathBuf::from(format!("{stem}{to}"));
                    if c.is_file() {
                        return Some(c);
                    }
                }
            }
        }
        for ext in EXTENSIONS {
            let c = PathBuf::from(format!("{s}{ext}"));
            if c.is_file() {
                return Some(c);
            }
        }
        if p.is_dir() {
            if let Some(pkg) = self.package_json(p) {
                let main = match kind {
                    Kind::Import => pkg.get("module").or_else(|| pkg.get("main")),
                    Kind::Require => pkg.get("main"),
                }
                .and_then(|v| v.as_str());
                if let Some(main) = main
                    && let Some(r) = self.resolve_file_or_index(&normalize(&p.join(main)))
                {
                    return Some(r);
                }
            }
            for ext in EXTENSIONS {
                let c = p.join(format!("index{ext}"));
                if c.is_file() {
                    return Some(c);
                }
            }
        }
        None
    }

    fn resolve_file_or_index(&self, p: &Path) -> Option<PathBuf> {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
        let s = p.to_string_lossy();
        for ext in EXTENSIONS {
            let c = PathBuf::from(format!("{s}{ext}"));
            if c.is_file() {
                return Some(c);
            }
        }
        for ext in EXTENSIONS {
            let c = p.join(format!("index{ext}"));
            if c.is_file() {
                return Some(c);
            }
        }
        None
    }

    fn package_json(&self, dir: &Path) -> Option<Rc<Value>> {
        if let Some(v) = self.pkg_json.borrow().get(dir) {
            return v.clone();
        }
        let v = std::fs::read_to_string(dir.join("package.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .map(Rc::new);
        self.pkg_json.borrow_mut().insert(dir.to_path_buf(), v.clone());
        v
    }

    /// Nearest package.json `type` for a file.
    fn package_type(&self, file: &Path) -> Option<String> {
        let mut dir = file.parent();
        while let Some(d) = dir {
            if let Some(pkg) = self.package_json(d) {
                return pkg.get("type").and_then(|t| t.as_str()).map(String::from);
            }
            if d.file_name().is_some_and(|n| n == "node_modules") {
                return None;
            }
            dir = d.parent();
        }
        None
    }

    fn resolve_package(&self, spec: &str, base_dir: &Path, kind: Kind) -> Option<PathBuf> {
        let (name, subpath) = split_package(spec)?;
        let mut dir = Some(base_dir.to_path_buf());
        while let Some(d) = dir {
            let candidate = d.join("node_modules").join(name);
            if candidate.is_dir() {
                return self.resolve_in_package(&candidate, subpath, kind);
            }
            // Self-reference: a package importing itself by name.
            if let Some(pkg) = self.package_json(&d)
                && pkg.get("name").and_then(|n| n.as_str()) == Some(name)
                && pkg.get("exports").is_some()
            {
                return self.resolve_in_package(&d, subpath, kind);
            }
            dir = d.parent().map(Path::to_path_buf);
        }
        None
    }

    fn resolve_in_package(&self, pkg_dir: &Path, subpath: &str, kind: Kind) -> Option<PathBuf> {
        let pkg = self.package_json(pkg_dir);
        if let Some(exports) = pkg.as_ref().and_then(|p| p.get("exports")) {
            let conditions: &[&str] = match kind {
                Kind::Import => &["import", "module", "node", "default"],
                Kind::Require => &["require", "node", "default"],
            };
            let sub = if subpath.is_empty() { ".".to_string() } else { format!("./{subpath}") };
            if let Some(target) = resolve_exports(exports, &sub, conditions) {
                let p = normalize(&pkg_dir.join(target));
                if p.is_file() {
                    return Some(p);
                }
                if let Some(p) = self.resolve_file_or_index(&p) {
                    return Some(p);
                }
            }
            // Lenient fallback (Node would throw ERR_PACKAGE_PATH_NOT_EXPORTED).
        }
        if subpath.is_empty() {
            self.resolve_path(pkg_dir, kind)
        } else {
            self.resolve_path(&normalize(&pkg_dir.join(subpath)), kind)
        }
    }

    // -----------------------------------------------------------------------
    // Loading
    // -----------------------------------------------------------------------

    fn prepare_file(&self, ctx: &mut Context, path: &Path) -> Result<Prepared, Error> {
        let source = self.shared.file_sources.read().get(path).cloned();
        let text = match source {
            Some(text) => text,
            None => std::fs::read_to_string(path).map_err(|e| Error::host(format!("cannot read {}: {e}", path.display())))?,
        };
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let in_node_modules = path.components().any(|c| c.as_os_str() == "node_modules");
        match ext {
            "json" => {
                return Ok(Prepared {
                    code: text,
                    format: Format::Json,
                });
            }
            "cjs" => {
                return Ok(Prepared {
                    code: strip_shebang(text),
                    format: Format::Cjs,
                });
            }
            "ts" | "mts" | "cts" | "tsx" => {
                let out = transpile::transpile(&text, path, SourceLang::from_path(path))?;
                if let Some(map) = out.source_map {
                    state(ctx).source_maps.borrow_mut().insert(path.display().to_string(), map);
                }
                let format = match ext {
                    "mts" => Format::Esm,
                    "cts" => Format::Cjs,
                    _ if out.has_module_syntax => Format::Esm,
                    _ => Format::Cjs,
                };
                let code = if format == Format::Esm && !in_node_modules {
                    self.cjs_globals(&out.code, &out.declared_cjs_names)
                } else {
                    out.code
                };
                return Ok(Prepared { code, format });
            }
            _ => {}
        }
        let text = strip_shebang(text);
        let pkg_type = self.package_type(path);
        if ext == "mjs" || pkg_type.as_deref() == Some("module") {
            if in_node_modules {
                return Ok(Prepared {
                    code: text,
                    format: Format::Esm,
                });
            }
            let out = transpile::transpile(&text, path, SourceLang::JavaScript)?;
            return Ok(Prepared {
                code: self.cjs_globals(&text, &out.declared_cjs_names),
                format: Format::Esm,
            });
        }
        if in_node_modules && !transpile::may_have_module_syntax(&text) {
            return Ok(Prepared {
                code: text,
                format: Format::Cjs,
            });
        }
        let out = transpile::transpile(&text, path, SourceLang::JavaScript)?;
        if out.has_module_syntax {
            let code = if in_node_modules {
                text
            } else {
                self.cjs_globals(&text, &out.declared_cjs_names)
            };
            Ok(Prepared { code, format: Format::Esm })
        } else {
            Ok(Prepared {
                code: text,
                format: Format::Cjs,
            })
        }
    }

    /// Create the QuickJS module named `name` (called by QuickJS's loader hook).
    fn load_module(self: &Rc<Self>, ctx: &mut Context, name: &str) -> Result<*mut qjs::JSModuleDef, Error> {
        tracing::trace!(target: "pi_jsrt", "load module {name}");
        let source = self.sources.borrow_mut().remove(name);
        if let Some(code) = source {
            return compile_module(ctx, &code, name);
        }
        let resolved = self.names.borrow().get(name).cloned().ok_or_else(|| Error::ModuleNotFound {
            specifier: name.to_string(),
            referrer: "<loader>".into(),
        })?;
        let key = name.to_string();
        let module = match &resolved {
            Resolved::Builtin(builtin) => {
                let value = require_builtin(ctx, builtin).map_err(|e| error::from_js(e, ctx))?;
                self.synthetic_from_value(ctx, value, name, false).map_err(|e| error::from_js(e, ctx))?
            }
            Resolved::Virtual(spec) => {
                let vm = self.shared.virtual_modules.read().get(spec).cloned();
                let vm = vm.ok_or_else(|| Error::ModuleNotFound {
                    specifier: spec.clone(),
                    referrer: "<virtual>".into(),
                })?;
                match vm {
                    VirtualModule::Esm(src) => {
                        let out = transpile::transpile(&src, Path::new(&format!("{spec}.ts")), SourceLang::TypeScript)?;
                        if let Some(map) = out.source_map {
                            state(ctx).source_maps.borrow_mut().insert(name.to_string(), map);
                        }
                        compile_module(ctx, &out.code, name)?
                    }
                    VirtualModule::EsmJs(src) => compile_module(ctx, &src, name)?,
                    _ => {
                        let value = self.require_resolved(ctx, &resolved).map_err(|e| error::from_js(e, ctx))?;
                        self.synthetic_from_value(ctx, value, name, true).map_err(|e| error::from_js(e, ctx))?
                    }
                }
            }
            Resolved::File(path) => {
                let prepared = self.prepare_file(ctx, path)?;
                match prepared.format {
                    Format::Esm => compile_module(ctx, &prepared.code, name).map_err(|e| with_file(e, path))?,
                    Format::Json => {
                        let v = parse_json_text(ctx, &prepared.code, path)?;
                        self.synthetic_from_value(ctx, v, name, false).map_err(|e| error::from_js(e, ctx))?
                    }
                    Format::Cjs => {
                        let v = self
                            .run_cjs(ctx, &key, path, &prepared.code)
                            .map_err(|e| error::from_js(e, ctx))?;
                        self.synthetic_from_value(ctx, v, name, true).map_err(|e| error::from_js(e, ctx))?
                    }
                }
            }
        };
        Ok(module)
    }

    /// A module exposing `value` as `default` and its own enumerable properties as named
    /// exports. With `cjs_interop`, `__esModule` objects use their `default` property as the
    /// default export.
    fn synthetic_from_value(&self, ctx: &mut Context, value: JsValue, name: &str, cjs_interop: bool) -> JsResult<*mut qjs::JSModuleDef> {
        let mut names = vec!["default".to_string()];
        let mut default = value.clone();
        if let Some(obj) = value.as_object() {
            // Like Node's CJS export detection, element indices of arrays and typed arrays (a
            // module exporting a 50 KB Buffer) are not named exports.
            let indexed = obj.is_array() || crate::js::JsTypedArray::from_object(obj.clone()).is_ok();
            for k in object_keys(ctx, &obj)? {
                let k = k.to_std_string_lossy();
                if k == "default" || k.contains('\0') {
                    continue;
                }
                if indexed && k.bytes().all(|b| b.is_ascii_digit()) {
                    continue;
                }
                names.push(k);
            }
            if cjs_interop && obj.get(js_string!("__esModule"), ctx)?.to_boolean() && obj.has_property(js_string!("default"), ctx)? {
                default = obj.get(js_string!("default"), ctx)?;
            }
        }
        let cname = CString::new(name).map_err(|_| engine::type_error("invalid module name"))?;
        let raw = ctx.as_raw();
        let m = unsafe { qjs::JS_NewCModule(raw, cname.as_ptr(), Some(synthetic_init)) };
        if m.is_null() {
            return Err(crate::js::take_exception(raw));
        }
        for export in &names {
            let c = CString::new(export.as_str()).unwrap_or_default();
            if unsafe { qjs::JS_AddModuleExport(raw, m, c.as_ptr()) } < 0 {
                return Err(crate::js::take_exception(raw));
            }
        }
        self.synthetic.borrow_mut().insert(name.to_string(), Synthetic { value, default, names });
        Ok(m)
    }

    /// `require()` a resolved target.
    fn require_resolved(self: &Rc<Self>, ctx: &mut Context, resolved: &Resolved) -> JsResult<JsValue> {
        let key = self.key(resolved);
        if let Some(m) = self.cjs.borrow().get(&key).cloned() {
            return m.get(js_string!("exports"), ctx);
        }
        if let Some(v) = self.values.borrow().get(&key).cloned() {
            return Ok(v);
        }
        match resolved {
            Resolved::Builtin(name) => require_builtin(ctx, name),
            Resolved::Virtual(spec) => {
                let vm = self.shared.virtual_modules.read().get(spec).cloned();
                let Some(vm) = vm else {
                    return Err(JsNativeError::error().with_message(format!("Cannot find module '{spec}'")).into());
                };
                match vm {
                    VirtualModule::Esm(_) | VirtualModule::EsmJs(_) => self.require_esm(ctx, resolved),
                    VirtualModule::CommonJs(src) => {
                        let path = PathBuf::from(format!("virtual:{spec}"));
                        self.run_cjs(ctx, &key, &path, &src)
                    }
                    VirtualModule::Json(v) => {
                        let v = JsValue::from_json(&v, ctx)?;
                        self.values.borrow_mut().insert(key, v.clone());
                        Ok(v)
                    }
                    VirtualModule::Expr(expr) => {
                        let path = PathBuf::from(format!("virtual:{spec}"));
                        let code = format!("({expr}\n)");
                        let v = ctx.eval(Source::from_bytes(code.as_bytes()).with_path(&path))?;
                        self.values.borrow_mut().insert(key, v.clone());
                        Ok(v)
                    }
                    VirtualModule::Object(handle) => {
                        let v = state(ctx)
                            .get_handle(handle.id())
                            .ok_or_else(|| engine::type_error(format!("virtual module '{spec}' refers to a released handle")))?;
                        self.values.borrow_mut().insert(key, v.clone());
                        Ok(v)
                    }
                }
            }
            Resolved::File(path) => {
                let prepared = self.prepare_file(ctx, path).map_err(|e| error::to_js(&e, ctx))?;
                match prepared.format {
                    Format::Json => {
                        let v = parse_json_text(ctx, &prepared.code, path).map_err(|e| error::to_js(&e, ctx))?;
                        self.values.borrow_mut().insert(key, v.clone());
                        Ok(v)
                    }
                    Format::Cjs => self.run_cjs(ctx, &key, path, &prepared.code),
                    Format::Esm => self.require_esm(ctx, resolved),
                }
            }
        }
    }

    /// Synchronously evaluate an ES module for `require()` and return its namespace.
    fn require_esm(self: &Rc<Self>, ctx: &mut Context, resolved: &Resolved) -> JsResult<JsValue> {
        let name = self.module_name(resolved);
        let promise = load_by_name(ctx, &name)?;
        while let Some(r) = ctx.run_pending_job() {
            if let Err(e) = r {
                engine::report_uncaught(ctx, e);
            }
        }
        match promise.state() {
            PromiseState::Fulfilled(ns) => Ok(ns),
            PromiseState::Rejected(e) => Err(JsError::from_opaque(e)),
            PromiseState::Pending => Err(JsNativeError::error()
                .with_message(format!(
                    "require() of ES module {} which uses top-level await is not supported; use import()",
                    module_path(&name)
                ))
                .into()),
        }
    }

    fn run_cjs(self: &Rc<Self>, ctx: &mut Context, key: &str, path: &Path, code: &str) -> JsResult<JsValue> {
        let filename = path.display().to_string();
        let create = internal(ctx).get(js_string!("createCjsModule"), ctx)?;
        let create = create
            .as_function()
            .ok_or_else(|| engine::type_error("createCjsModule missing"))?;
        let module_obj = create
            .call(&JsValue::undefined(), &[js_str(&filename)], ctx)?
            .as_object()
            .ok_or_else(|| engine::type_error("createCjsModule returned a non-object"))?;
        self.cjs.borrow_mut().insert(key.to_string(), module_obj.clone());
        let wrapped = format!("(function (exports, require, module, __filename, __dirname) {{{code}\n}})");
        let result = (|| {
            let f = ctx.eval(Source::from_bytes(wrapped.as_bytes()).with_path(path))?;
            let f = f.as_function().ok_or_else(|| engine::type_error("CommonJS wrapper is not a function"))?;
            let exports = module_obj.get(js_string!("exports"), ctx)?;
            let require = module_obj.get(js_string!("require"), ctx)?;
            let dirname = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
            f.call(
                &exports,
                &[exports.clone(), require, module_obj.clone().into(), js_str(&filename), js_str(&dirname)],
                ctx,
            )?;
            module_obj.set(js_string!("loaded"), true, false, ctx)?;
            module_obj.get(js_string!("exports"), ctx)
        })();
        if result.is_err() {
            self.cjs.borrow_mut().remove(key);
        }
        result
    }
}

const EXTENSIONS: &[&str] = &[".ts", ".tsx", ".mts", ".cts", ".js", ".mjs", ".cjs", ".json"];

fn strip_shebang(text: String) -> String {
    match text.strip_prefix("#!") {
        Some(rest) => format!("//{rest}"),
        None => text,
    }
}

/// Prefix ES module code with jiti-compatible CommonJS bindings (on line 1, so
/// line numbers are unchanged).
fn inject_cjs_globals(code: &str, declared: &[&str]) -> String {
    // A hashbang must stay first; turn it into a comment so the prefix can go in front.
    let code = match code.strip_prefix("#!") {
        Some(rest) => std::borrow::Cow::Owned(format!("//{rest}")),
        None => std::borrow::Cow::Borrowed(code),
    };
    let code = code.as_ref();
    let mut prefix = String::new();
    if !declared.contains(&"__filename") {
        prefix.push_str("const __filename = import.meta.filename; ");
    }
    if !declared.contains(&"__dirname") {
        prefix.push_str("const __dirname = import.meta.dirname; ");
    }
    if !declared.contains(&"require") {
        prefix.push_str("const require = globalThis[Symbol.for(\"pi-jsrt.createRequire\")](import.meta.filename); ");
    }
    if prefix.is_empty() {
        return code.to_string();
    }
    format!("{prefix}{code}")
}

fn split_package(spec: &str) -> Option<(&str, &str)> {
    if spec.is_empty() {
        return None;
    }
    let idx = if spec.starts_with('@') {
        let first = spec.find('/')?;
        spec[first + 1..].find('/').map(|i| first + 1 + i)
    } else {
        spec.find('/')
    };
    Some(match idx {
        Some(i) => (&spec[..i], &spec[i + 1..]),
        None => (spec, ""),
    })
}

/// Resolve a package.json `exports` entry for `subpath` ("." or "./x").
pub(crate) fn resolve_exports(exports: &Value, subpath: &str, conditions: &[&str]) -> Option<String> {
    let is_subpath_map = exports
        .as_object()
        .is_some_and(|m| m.keys().next().is_some_and(|k| k.starts_with('.') || k.starts_with('#')));
    if !is_subpath_map {
        return if subpath == "." { resolve_target(exports, None, conditions) } else { None };
    }
    let map = exports.as_object()?;
    if let Some(t) = map.get(subpath) {
        return resolve_target(t, None, conditions);
    }
    // Pattern keys: "./features/*.js"
    let mut best: Option<(&str, &Value, String)> = None;
    for (key, target) in map {
        let Some(star) = key.find('*') else { continue };
        let (prefix, suffix) = (&key[..star], &key[star + 1..]);
        if subpath.len() >= prefix.len() + suffix.len() && subpath.starts_with(prefix) && subpath.ends_with(suffix) {
            let matched = &subpath[prefix.len()..subpath.len() - suffix.len()];
            if best.as_ref().is_none_or(|(k, _, _)| key.len() > k.len()) {
                best = Some((key, target, matched.to_string()));
            }
        }
    }
    let (_, target, matched) = best?;
    resolve_target(target, Some(&matched), conditions)
}

fn resolve_target(target: &Value, matched: Option<&str>, conditions: &[&str]) -> Option<String> {
    match target {
        Value::String(s) => Some(match matched {
            Some(m) => s.replace('*', m),
            None => s.clone(),
        }),
        Value::Array(items) => items.iter().find_map(|t| resolve_target(t, matched, conditions)),
        Value::Object(map) => {
            for (cond, t) in map {
                if (cond == "default" || conditions.contains(&cond.as_str()))
                    && let Some(r) = resolve_target(t, matched, conditions)
                {
                    return Some(r);
                }
            }
            None
        }
        _ => None,
    }
}

/// Lexically normalize a path (resolve `.` and `..`).
pub(crate) fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Builtins that, as in Node, exist only under the `node:` scheme (`sqlite` is an npm package).
const PREFIX_ONLY_BUILTINS: &[&str] = &["sqlite"];

fn builtin_name(ctx: &mut Context, spec: &str) -> Option<String> {
    let name = match spec.strip_prefix("node:") {
        Some(name) => name,
        None if PREFIX_ONLY_BUILTINS.contains(&spec) => return None,
        None => spec,
    };
    let has = internal(ctx).get(js_string!("hasBuiltin"), ctx).ok()?.as_function()?;
    let r = has.call(&JsValue::undefined(), &[js_str(name)], ctx).ok()?;
    if r.to_boolean() { Some(name.to_string()) } else { None }
}

fn require_builtin(ctx: &mut Context, name: &str) -> JsResult<JsValue> {
    let f = internal(ctx).get(js_string!("requireBuiltin"), ctx)?;
    let f = f.as_function().ok_or_else(|| engine::type_error("requireBuiltin missing"))?;
    f.call(&JsValue::undefined(), &[js_str(name)], ctx)
}

fn parse_json_text(ctx: &mut Context, text: &str, path: &Path) -> Result<JsValue, Error> {
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| Error::Syntax {
        file: path.display().to_string(),
        message: format!("{}: {e}", path.display()),
    })?;
    JsValue::from_json(&v, ctx).map_err(|e| error::from_js(e, ctx))
}

fn object_keys(ctx: &mut Context, obj: &JsObject) -> JsResult<Vec<JsString>> {
    let object_ctor = ctx.intrinsics().constructors().object().constructor();
    let keys = object_ctor.get(js_string!("keys"), ctx)?;
    let keys = keys.as_function().ok_or_else(|| engine::type_error("Object.keys missing"))?;
    let arr = keys.call(&JsValue::undefined(), &[obj.clone().into()], ctx)?;
    let arr = JsArray::from_object(arr.as_object().ok_or_else(|| engine::type_error("keys"))?)?;
    let len = arr.length(ctx)?;
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        out.push(arr.get(i, ctx)?.to_string(ctx)?);
    }
    Ok(out)
}

/// Name the file in parse errors.
fn with_file(err: Error, path: &Path) -> Error {
    match err {
        Error::Js(mut e) if e.name == "SyntaxError" && !e.message.contains(&*path.to_string_lossy()) => {
            e.message = format!("{} ({})", e.message, path.display());
            Error::Js(e)
        }
        other => other,
    }
}

fn referrer_dir(ctx: &Context, path: Option<&Path>) -> PathBuf {
    match path {
        Some(p) if p.is_absolute() => p.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/")),
        _ => state(ctx).cwd.borrow().clone(),
    }
}

/// The directory imports of module `name` resolve against.
fn base_dir_of(ctx: &Context, name: &str) -> PathBuf {
    let path = module_path(name);
    let path = path.strip_prefix("file://").unwrap_or(path);
    referrer_dir(ctx, (!path.is_empty()).then_some(Path::new(path)))
}

fn loader(ctx: &Context) -> Rc<Loader> {
    ctx.get_data::<Rc<Loader>>().cloned().expect("pi-jsrt loader not installed")
}

/// Install the loader in the engine (QuickJS module hooks).
pub(crate) fn install(engine: &Engine, loader: Rc<Loader>) {
    engine.context().insert_data(loader);
    unsafe {
        qjs::JS_SetModuleLoaderFunc2(engine.raw_runtime(), Some(normalize_hook), Some(loader_hook), Some(check_attributes_hook), std::ptr::null_mut());
    }
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

/// QuickJS `module_normalize`: resolve `name` imported by module `base` to a module name.
unsafe extern "C" fn normalize_hook(raw: *mut qjs::JSContext, base: *const c_char, name: *const c_char, _opaque: *mut c_void) -> *mut c_char {
    let base = cstr(base);
    let spec = cstr(name);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut ctx = Context::from_raw(raw);
        let l = loader(&ctx);
        if l.names.borrow().contains_key(&spec) || l.sources.borrow().contains_key(&spec) {
            return Ok(spec.clone());
        }
        let dir = base_dir_of(&ctx, &base);
        let resolved = l.resolve(&mut ctx, &spec, &dir, Kind::Import).map_err(|e| error::to_js(&e, &mut ctx))?;
        Ok::<_, JsError>(l.module_name(&resolved))
    }));
    match r {
        Ok(Ok(resolved)) => {
            let c = CString::new(resolved).unwrap_or_default();
            unsafe { qjs::js_strdup(raw, c.as_ptr()) }
        }
        Ok(Err(e)) => {
            crate::js::throw(raw, e);
            std::ptr::null_mut()
        }
        Err(_) => {
            crate::js::throw(raw, engine::type_error(format!("panic while resolving '{spec}'")));
            std::ptr::null_mut()
        }
    }
}

/// QuickJS `module_loader`: create the module named `name`.
unsafe extern "C" fn loader_hook(raw: *mut qjs::JSContext, name: *const c_char, _opaque: *mut c_void, _attributes: qjs::JSValue) -> *mut qjs::JSModuleDef {
    let name = cstr(name);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut ctx = Context::from_raw(raw);
        let l = loader(&ctx);
        l.loaded.borrow_mut().insert(name.clone());
        l.load_module(&mut ctx, &name).map_err(|e| error::to_js(&e, &mut ctx))
    }));
    match r {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => {
            crate::js::throw(raw, e);
            std::ptr::null_mut()
        }
        Err(_) => {
            crate::js::throw(raw, engine::type_error(format!("panic while loading '{name}'")));
            std::ptr::null_mut()
        }
    }
}

/// Import attributes are accepted as is (JSON modules are recognized by extension).
unsafe extern "C" fn check_attributes_hook(_ctx: *mut qjs::JSContext, _opaque: *mut c_void, _attributes: qjs::JSValue) -> c_int {
    0
}

/// Initialize a module made by [`Loader::synthetic_from_value`]: set its exports.
unsafe extern "C" fn synthetic_init(raw: *mut qjs::JSContext, m: *mut qjs::JSModuleDef) -> c_int {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> JsResult<()> {
        let mut ctx = Context::from_raw(raw);
        let atom = unsafe { qjs::JS_GetModuleName(raw, m) };
        let name_val = unsafe { JsValue::from_raw(qjs::JS_AtomToValue(raw, atom)) };
        unsafe { qjs::JS_FreeAtom(raw, atom) };
        let name = name_val.to_string(&mut ctx)?.to_std_string_lossy();
        let syn = loader(&ctx).synthetic.borrow_mut().remove(&name);
        let Some(syn) = syn else { return Ok(()) };
        let obj = syn.value.as_object();
        for export in &syn.names {
            let v = if export == "default" {
                syn.default.clone()
            } else if let Some(obj) = &obj {
                obj.get(export.as_str(), &mut ctx)?
            } else {
                JsValue::undefined()
            };
            let c = CString::new(export.as_str()).unwrap_or_default();
            if unsafe { qjs::JS_SetModuleExport(raw, m, c.as_ptr(), v.into_raw()) } < 0 {
                return Err(crate::js::take_exception(raw));
            }
        }
        Ok(())
    }));
    match r {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            crate::js::throw(raw, e);
            -1
        }
        Err(_) => -1,
    }
}

/// Compile ES module source named `name` and fill its `import.meta`.
fn compile_module(ctx: &mut Context, code: &str, name: &str) -> Result<*mut qjs::JSModuleDef, Error> {
    let flags = (qjs::JS_EVAL_TYPE_MODULE | qjs::JS_EVAL_FLAG_COMPILE_ONLY) as i32;
    let value = ctx.eval_with_flags(code.as_bytes(), name, flags).map_err(|e| error::from_js(e, ctx))?;
    let m = unsafe { qjs::JS_VALUE_GET_PTR(value.as_raw()) } as *mut qjs::JSModuleDef;
    let meta = unsafe { JsValue::from_raw(qjs::JS_GetImportMeta(ctx.as_raw(), m)) };
    if let Some(meta) = meta.as_object() {
        init_import_meta(ctx, &meta, name);
    }
    drop(value);
    Ok(m)
}

fn init_import_meta(ctx: &mut Context, import_meta: &JsObject, name: &str) {
    let path = Path::new(module_path(name));
    let path_str = path.display().to_string();
    let url = if path.is_absolute() {
        url::Url::from_file_path(path).map(|u| u.to_string()).unwrap_or_else(|_| path_str.clone())
    } else {
        path_str.clone()
    };
    let _ = import_meta.set(js_string!("url"), js_str(&url), false, ctx);
    if path.is_absolute() {
        let dir = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
        let _ = import_meta.set(js_string!("filename"), js_str(&path_str), false, ctx);
        let _ = import_meta.set(js_string!("dirname"), js_str(&dir), false, ctx);
    }
    if let Ok(f) = internal(ctx).get(js_string!("makeImportMetaResolve"), ctx)
        && let Some(f) = f.as_function()
        && let Ok(resolve) = f.call(&JsValue::undefined(), &[js_str(&path_str)], ctx)
    {
        let _ = import_meta.set(js_string!("resolve"), resolve, false, ctx);
    }
}

/// Load, link and evaluate the module named `name` (like `import()`): a promise of its namespace.
fn load_by_name(ctx: &mut Context, name: &str) -> JsResult<JsPromise> {
    let c = CString::new(name).map_err(|_| engine::type_error("invalid module name"))?;
    let raw = ctx.as_raw();
    let v = crate::js::check(raw, unsafe { qjs::JS_LoadModule(raw, c"".as_ptr(), c.as_ptr()) })?;
    v.as_promise().ok_or_else(|| engine::type_error("JS_LoadModule did not return a promise"))
}

// ---------------------------------------------------------------------------
// Entry points used by the runtime API and ops
// ---------------------------------------------------------------------------

/// Evaluate a classic script.
pub(crate) fn eval_script(ctx: &mut Context, code: &str, filename: &str) -> JsResult<JsValue> {
    let cwd = state(ctx).cwd.borrow().clone();
    let path = if filename.starts_with('<') || filename.starts_with('[') { PathBuf::from(filename) } else { cwd.join(filename) };
    let code = if filename.ends_with(".ts") || filename.ends_with(".mts") || filename.ends_with(".cts") {
        let out = transpile::transpile(code, &path, SourceLang::TypeScript).map_err(|e| error::to_js(&e, ctx))?;
        if let Some(map) = out.source_map {
            state(ctx).source_maps.borrow_mut().insert(path.display().to_string(), map);
        }
        out.code
    } else {
        code.to_string()
    };
    ctx.eval(Source::from_bytes(code.as_bytes()).with_path(&path))
}

/// An ES module from source text (evaluated by [`evaluate_module`]).
pub(crate) fn module_from_source(ctx: &mut Context, source: &str, filename: &str) -> Result<Module, Error> {
    let cwd = state(ctx).cwd.borrow().clone();
    let path = normalize(&cwd.join(filename));
    let lang = SourceLang::from_path(&path);
    let out = transpile::transpile(source, &path, lang)?;
    if let Some(map) = out.source_map {
        state(ctx).source_maps.borrow_mut().insert(path.display().to_string(), map);
    }
    let l = loader(ctx);
    let code = l.cjs_globals(&out.code, &out.declared_cjs_names);
    // A name QuickJS already loaded would return that module: use a fresh one.
    let mut name = path.display().to_string();
    if l.loaded.borrow().contains(&name) || l.sources.borrow().contains_key(&name) {
        l.evals.set(l.evals.get() + 1);
        name = format!("{name}{EVAL_MARK}{}", l.evals.get());
    }
    l.sources.borrow_mut().insert(name.clone(), code);
    Ok(Module(name))
}

/// Resolve the module for `specifier` (relative to `referrer` or the cwd).
pub(crate) fn module_for_specifier(ctx: &mut Context, specifier: &str, referrer: Option<&Path>) -> Result<Module, Error> {
    let l = loader(ctx);
    let base = referrer_dir(ctx, referrer);
    let resolved = l.resolve(ctx, specifier, &base, Kind::Import)?;
    Ok(Module(l.module_name(&resolved)))
}

/// Load, link and evaluate `module`; `done` receives the namespace or the error.
pub(crate) fn evaluate_module(ctx: &mut Context, module: Module, done: impl FnOnce(&mut Context, Result<JsValue, JsValue>) + 'static) {
    let promise = load_by_name(ctx, &module.0).map(JsValue::from);
    engine::when_settled(ctx, promise, done);
}

/// `require(specifier)` from `parent` (a filename) or the cwd.
pub(crate) fn require(ctx: &mut Context, specifier: &str, parent: Option<&Path>) -> JsResult<JsValue> {
    let l = loader(ctx);
    let base = referrer_dir(ctx, parent);
    let resolved = l.resolve(ctx, specifier, &base, Kind::Require).map_err(|e| error::to_js(&e, ctx))?;
    l.require_resolved(ctx, &resolved)
}

pub(crate) fn clear_file_cache(ctx: &mut Context) {
    loader(ctx).clear_files();
}

fn op_require(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let spec = arg_string(args, 0, ctx)?;
    let parent = arg_opt_string(args, 1, ctx)?;
    require(ctx, &spec, parent.as_deref().map(Path::new))
}

fn op_require_resolve(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let spec = arg_string(args, 0, ctx)?;
    let parent = arg_opt_string(args, 1, ctx)?;
    let kind = if args.get_or_undefined(2).to_boolean() { Kind::Import } else { Kind::Require };
    let l = loader(ctx);
    let base = referrer_dir(ctx, parent.as_deref().map(Path::new));
    match l.resolve(ctx, &spec, &base, kind).map_err(|e| error::to_js(&e, ctx))? {
        Resolved::Builtin(n) => Ok(js_str(if n.starts_with("jsrt:") { n.clone() } else { format!("node:{n}") }.as_str())),
        Resolved::Virtual(s) => Ok(js_str(&s)),
        Resolved::File(p) => Ok(js_str(&p.display().to_string())),
    }
}

fn op_set_cwd(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dir = arg_string(args, 0, ctx)?;
    let st = state(ctx);
    let new = normalize(&st.cwd.borrow().join(&dir));
    if !new.is_dir() {
        let err = Error::Js(crate::error::JsException {
            name: "Error".into(),
            message: format!("ENOENT: no such file or directory, chdir '{dir}'"),
            stack: None,
            code: Some("ENOENT".into()),
        });
        return Err(error::to_js(&err, ctx));
    }
    *st.cwd.borrow_mut() = new;
    Ok(JsValue::undefined())
}

fn op_get_cwd(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(js_str(&state(ctx).cwd.borrow().display().to_string()))
}

fn op_transpile(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let code = arg_string(args, 0, ctx)?;
    let name = arg_opt_string(args, 1, ctx)?.unwrap_or_else(|| "input.ts".into());
    let out = transpile::transpile(&code, Path::new(&name), SourceLang::from_path(Path::new(&name))).map_err(|e| error::to_js(&e, ctx))?;
    Ok(js_str(&out.code))
}

/// `ops.isCommonJs(path) -> bool`: whether a file loads as CommonJS (Node mode runs a CommonJS
/// main module synchronously, like Node).
fn op_is_common_js(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let path = arg_string(args, 0, ctx)?;
    let l = loader(ctx);
    let Some(resolved) = l.resolve_path(Path::new(&path), Kind::Require) else { return Ok(JsValue::from(false)) };
    let cjs = l.prepare_file(ctx, &resolved).is_ok_and(|p| p.format == Format::Cjs);
    Ok(JsValue::from(cjs))
}

/// `ops.evalScript(code, filename)`: evaluate a classic script (`node -e`, `vm.runInThisContext`).
fn op_eval_script(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let code = arg_string(args, 0, ctx)?;
    let name = arg_opt_string(args, 1, ctx)?.unwrap_or_else(|| "<eval>".into());
    eval_script(ctx, &code, &name)
}

pub(crate) const LOADER_OPS: &[(&str, usize, engine::OpFn)] = &[
    ("evalScript", 2, op_eval_script),
    ("isCommonJs", 1, op_is_common_js),
    ("require", 2, op_require),
    ("requireResolve", 3, op_require_resolve),
    ("chdir", 1, op_set_cwd),
    ("cwd", 0, op_get_cwd),
    ("transpile", 2, op_transpile),
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exports_resolution() {
        let exports = json!({
            ".": { "types": "./d.ts", "import": "./esm/index.js", "require": "./cjs/index.js" },
            "./compile": { "import": "./esm/compile.js", "default": "./cjs/compile.js" },
            "./features/*": "./src/features/*.js",
            "./package.json": "./package.json"
        });
        let imp = &["import", "node", "default"];
        let req = &["require", "node", "default"];
        assert_eq!(resolve_exports(&exports, ".", imp).as_deref(), Some("./esm/index.js"));
        assert_eq!(resolve_exports(&exports, ".", req).as_deref(), Some("./cjs/index.js"));
        assert_eq!(resolve_exports(&exports, "./compile", req).as_deref(), Some("./cjs/compile.js"));
        assert_eq!(resolve_exports(&exports, "./features/a/b", imp).as_deref(), Some("./src/features/a/b.js"));
        assert_eq!(resolve_exports(&json!("./main.js"), ".", imp).as_deref(), Some("./main.js"));
        assert_eq!(resolve_exports(&json!({"import": "./m.mjs", "default": "./m.cjs"}), ".", req).as_deref(), Some("./m.cjs"));
        assert_eq!(resolve_exports(&exports, "./missing", imp), None);
    }

    #[test]
    fn package_split() {
        assert_eq!(split_package("ms"), Some(("ms", "")));
        assert_eq!(split_package("typebox/value"), Some(("typebox", "value")));
        assert_eq!(split_package("@scope/pkg"), Some(("@scope/pkg", "")));
        assert_eq!(split_package("@scope/pkg/a/b"), Some(("@scope/pkg", "a/b")));
    }

    #[test]
    fn injection_skips_declared() {
        let out = inject_cjs_globals("x", &["require"]);
        assert!(out.contains("__filename"));
        assert!(!out.contains("const require"));
    }
}
