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

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use boa_engine::module::{ModuleLoader, Referrer, SyntheticModuleInitializer};
use boa_engine::object::builtins::JsArray;
use boa_engine::{
    Context, JsArgs, JsError, JsNativeError, JsObject, JsResult, JsString, JsValue, Module, Script, Source, js_string,
};
use boa_engine::builtins::promise::PromiseState;
use boa_engine::module::ModuleRequest;
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
    fn key(&self) -> String {
        match self {
            Resolved::Builtin(n) => format!("builtin:{n}"),
            Resolved::Virtual(s) => format!("virtual:{s}"),
            Resolved::File(p) => p.display().to_string(),
        }
    }
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

/// The module loader installed in the Boa context.
pub(crate) struct Loader {
    shared: Arc<Shared>,
    esm: RefCell<HashMap<String, Module>>,
    cjs: RefCell<HashMap<String, JsObject>>,
    values: RefCell<HashMap<String, JsValue>>,
    pending: RefCell<HashMap<u64, Module>>,
    pkg_json: RefCell<HashMap<PathBuf, Option<Rc<Value>>>>,
}

impl Loader {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Loader {
            shared,
            esm: RefCell::default(),
            cjs: RefCell::default(),
            values: RefCell::default(),
            pending: RefCell::default(),
            pkg_json: RefCell::default(),
        }
    }

    /// jiti-style `__filename`/`__dirname`/`require` for ES modules, unless disabled
    /// ([`crate::RuntimeOptions::esm_cjs_globals`]).
    fn cjs_globals(&self, code: &str, declared: &[&str]) -> String {
        if self.shared.options.esm_cjs_globals { inject_cjs_globals(code, declared) } else { code.to_string() }
    }

    pub(crate) fn clear(&self) {
        self.esm.borrow_mut().clear();
        self.cjs.borrow_mut().clear();
        self.values.borrow_mut().clear();
        self.pending.borrow_mut().clear();
    }

    fn clear_files(&self) {
        self.esm.borrow_mut().retain(|k, _| k.starts_with("builtin:") || k.starts_with("virtual:"));
        self.cjs.borrow_mut().retain(|k, _| k.starts_with("builtin:") || k.starts_with("virtual:"));
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

    /// Load (but do not evaluate) the ES module for a resolved target.
    fn load_module(self: &Rc<Self>, ctx: &mut Context, resolved: &Resolved) -> Result<Module, Error> {
        let key = resolved.key();
        tracing::trace!(target: "pi_jsrt", "load module {key}");
        if let Some(m) = self.esm.borrow().get(&key) {
            return Ok(m.clone());
        }
        let module = match resolved {
            Resolved::Builtin(name) => {
                let value = require_builtin(ctx, name).map_err(|e| error::from_boa(e, ctx))?;
                synthetic_from_value(ctx, value, None, false).map_err(|e| error::from_boa(e, ctx))?
            }
            Resolved::Virtual(spec) => {
                let vm = self.shared.virtual_modules.read().get(spec).cloned();
                let vm = vm.ok_or_else(|| Error::ModuleNotFound {
                    specifier: spec.clone(),
                    referrer: "<virtual>".into(),
                })?;
                match vm {
                    VirtualModule::Esm(src) => {
                        let path = PathBuf::from(format!("virtual:{spec}"));
                        let out = transpile::transpile(&src, Path::new(&format!("{spec}.ts")), SourceLang::TypeScript)?;
                        if let Some(map) = out.source_map {
                            state(ctx).source_maps.borrow_mut().insert(path.display().to_string(), map);
                        }
                        Module::parse(Source::from_bytes(out.code.as_bytes()).with_path(&path), None, ctx)
                            .map_err(|e| error::from_boa(e, ctx))?
                    }
                    VirtualModule::EsmJs(src) => {
                        let path = PathBuf::from(format!("virtual:{spec}"));
                        Module::parse(Source::from_bytes(src.as_bytes()).with_path(&path), None, ctx)
                            .map_err(|e| error::from_boa(e, ctx))?
                    }
                    _ => {
                        let value = self.require_resolved(ctx, resolved).map_err(|e| error::from_boa(e, ctx))?;
                        synthetic_from_value(ctx, value, None, true).map_err(|e| error::from_boa(e, ctx))?
                    }
                }
            }
            Resolved::File(path) => {
                let prepared = self.prepare_file(ctx, path)?;
                match prepared.format {
                    Format::Esm => Module::parse(Source::from_bytes(prepared.code.as_bytes()).with_path(path), None, ctx)
                        .map_err(|e| with_file(error::from_boa(e, ctx), path))?,
                    Format::Json => {
                        let v = parse_json_text(ctx, &prepared.code, path)?;
                        synthetic_from_value(ctx, v, Some(path.clone()), false).map_err(|e| error::from_boa(e, ctx))?
                    }
                    Format::Cjs => {
                        let v = self
                            .run_cjs(ctx, &key, path, &prepared.code)
                            .map_err(|e| error::from_boa(e, ctx))?;
                        synthetic_from_value(ctx, v, Some(path.clone()), true).map_err(|e| error::from_boa(e, ctx))?
                    }
                }
            }
        };
        self.esm.borrow_mut().insert(key, module.clone());
        Ok(module)
    }

    /// `require()` a resolved target.
    fn require_resolved(self: &Rc<Self>, ctx: &mut Context, resolved: &Resolved) -> JsResult<JsValue> {
        let key = resolved.key();
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
                let prepared = self.prepare_file(ctx, path).map_err(|e| error::to_boa(&e, ctx))?;
                match prepared.format {
                    Format::Json => {
                        let v = parse_json_text(ctx, &prepared.code, path).map_err(|e| error::to_boa(&e, ctx))?;
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
        let module = self.load_module(ctx, resolved).map_err(|e| error::to_boa(&e, ctx))?;
        let promise = module.load_link_evaluate(ctx);
        ctx.run_jobs()?;
        match promise.state() {
            PromiseState::Fulfilled(_) => Ok(module.namespace(ctx).into()),
            PromiseState::Rejected(e) => Err(JsError::from_opaque(e)),
            PromiseState::Pending => Err(JsNativeError::error()
                .with_message(format!(
                    "require() of ES module {} which uses top-level await is not supported; use import()",
                    resolved.key()
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
    JsValue::from_json(&v, ctx).map_err(|e| error::from_boa(e, ctx))
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

/// Build a synthetic ES module exposing `value` as `default` and its own enumerable
/// properties as named exports. With `cjs_interop`, `__esModule` objects use their
/// `default` property as the default export.
/// Name the file in parse errors (Boa's messages only carry line and column).
fn with_file(err: Error, path: &Path) -> Error {
    match err {
        Error::Js(mut e) if e.name == "SyntaxError" && !e.message.contains(&*path.to_string_lossy()) => {
            e.message = format!("{} ({})", e.message, path.display());
            Error::Js(e)
        }
        other => other,
    }
}

fn synthetic_from_value(ctx: &mut Context, value: JsValue, path: Option<PathBuf>, cjs_interop: bool) -> JsResult<Module> {
    let mut names = vec![js_string!("default")];
    let mut default = value.clone();
    if let Some(obj) = value.as_object() {
        // Like Node's CJS export detection, element indices of arrays and typed arrays (a
        // module exporting a 50 KB Buffer) are not named exports.
        let indexed = obj.is_array() || boa_engine::object::builtins::JsTypedArray::from_object(obj.clone()).is_ok();
        for k in object_keys(ctx, &obj)? {
            if k == js_string!("default") {
                continue;
            }
            if indexed && k.to_std_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            names.push(k);
        }
        if cjs_interop && obj.get(js_string!("__esModule"), ctx)?.to_boolean() && obj.has_property(js_string!("default"), ctx)? {
            default = obj.get(js_string!("default"), ctx)?;
        }
    }
    let names_arr = JsArray::from_iter(names.iter().map(|n| JsValue::from(n.clone())), ctx);
    let names_obj: JsObject = names_arr.into();
    Ok(Module::synthetic(
        &names,
        SyntheticModuleInitializer::from_copy_closure_with_captures(
            |m, (value, default, names): &(JsValue, JsValue, JsObject), ctx| {
                let len = names.get(js_string!("length"), ctx)?.to_length(ctx)?;
                let obj = value.as_object();
                for i in 0..len {
                    let name = names.get(i, ctx)?.to_string(ctx)?;
                    if name == js_string!("default") {
                        m.set_export(&name, default.clone())?;
                    } else if let Some(obj) = &obj {
                        let v = obj.get(name.clone(), ctx)?;
                        m.set_export(&name, v)?;
                    }
                }
                Ok(())
            },
            (value, default, names_obj),
        ),
        path,
        None,
        ctx,
    ))
}

fn referrer_dir(ctx: &Context, path: Option<&Path>) -> PathBuf {
    match path {
        Some(p) if p.is_absolute() => p.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/")),
        _ => state(ctx).cwd.borrow().clone(),
    }
}

impl ModuleLoader for Loader {
    #[allow(clippy::manual_async_fn)]
    fn load_imported_module(
        self: Rc<Self>,
        referrer: Referrer,
        request: ModuleRequest,
        context: &RefCell<&mut Context>,
    ) -> impl std::future::Future<Output = JsResult<Module>> {
        let result = (|| {
            let ctx = &mut **context.borrow_mut();
            let spec = request.specifier().to_std_string_lossy();
            let base = referrer_dir(ctx, referrer.path());
            let resolved = self.resolve(ctx, &spec, &base, Kind::Import).map_err(|e| error::to_boa(&e, ctx))?;
            self.load_module(ctx, &resolved).map_err(|e| error::to_boa(&e, ctx))
        })();
        async move { result }
    }

    fn init_import_meta(self: Rc<Self>, import_meta: &JsObject, module: &Module, context: &mut Context) {
        let Some(path) = module.path() else { return };
        let path_str = path.display().to_string();
        let url = if path.is_absolute() {
            url::Url::from_file_path(path).map(|u| u.to_string()).unwrap_or_else(|_| path_str.clone())
        } else {
            path_str.clone()
        };
        let _ = import_meta.set(js_string!("url"), js_str(&url), false, context);
        if path.is_absolute() {
            let dir = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
            let _ = import_meta.set(js_string!("filename"), js_str(&path_str), false, context);
            let _ = import_meta.set(js_string!("dirname"), js_str(&dir), false, context);
        }
        if let Ok(f) = internal(context).get(js_string!("makeImportMetaResolve"), context)
            && let Some(f) = f.as_function()
            && let Ok(resolve) = f.call(&JsValue::undefined(), &[js_str(&path_str)], context)
        {
            let _ = import_meta.set(js_string!("resolve"), resolve, false, context);
        }
    }
}

fn loader(ctx: &Context) -> Rc<Loader> {
    ctx.downcast_module_loader::<Loader>().expect("pi-jsrt loader not installed")
}

// ---------------------------------------------------------------------------
// Entry points used by the runtime API and ops
// ---------------------------------------------------------------------------

/// Evaluate a classic script.
pub(crate) fn eval_script(ctx: &mut Context, code: &str, filename: &str) -> JsResult<JsValue> {
    let cwd = state(ctx).cwd.borrow().clone();
    let path = if filename.starts_with('<') || filename.starts_with('[') { PathBuf::from(filename) } else { cwd.join(filename) };
    let code = if filename.ends_with(".ts") || filename.ends_with(".mts") || filename.ends_with(".cts") {
        let out = transpile::transpile(code, &path, SourceLang::TypeScript).map_err(|e| error::to_boa(&e, ctx))?;
        if let Some(map) = out.source_map {
            state(ctx).source_maps.borrow_mut().insert(path.display().to_string(), map);
        }
        out.code
    } else {
        code.to_string()
    };
    let script = Script::parse(Source::from_bytes(code.as_bytes()).with_path(&path), None, ctx)?;
    script.evaluate(ctx)
}

/// Parse an ES module from source text.
pub(crate) fn module_from_source(ctx: &mut Context, source: &str, filename: &str) -> Result<Module, Error> {
    let cwd = state(ctx).cwd.borrow().clone();
    let path = normalize(&cwd.join(filename));
    let lang = SourceLang::from_path(&path);
    let out = transpile::transpile(source, &path, lang)?;
    if let Some(map) = out.source_map {
        state(ctx).source_maps.borrow_mut().insert(path.display().to_string(), map);
    }
    let code = loader(ctx).cjs_globals(&out.code, &out.declared_cjs_names);
    let module = Module::parse(Source::from_bytes(code.as_bytes()).with_path(&path), None, ctx).map_err(|e| error::from_boa(e, ctx))?;
    loader(ctx).esm.borrow_mut().insert(path.display().to_string(), module.clone());
    Ok(module)
}

/// Resolve and load the module for `specifier` (relative to `referrer` or the cwd).
pub(crate) fn module_for_specifier(ctx: &mut Context, specifier: &str, referrer: Option<&Path>) -> Result<Module, Error> {
    let l = loader(ctx);
    let base = referrer_dir(ctx, referrer);
    let resolved = l.resolve(ctx, specifier, &base, Kind::Import)?;
    l.load_module(ctx, &resolved)
}

/// Load, link and evaluate `module`; `done` receives the namespace or the error.
pub(crate) fn evaluate_module(ctx: &mut Context, module: Module, done: impl FnOnce(&mut Context, Result<JsValue, JsValue>) + 'static) {
    let l = loader(ctx);
    let id = state(ctx).next_id();
    l.pending.borrow_mut().insert(id, module.clone());
    let promise = module.load_link_evaluate(ctx);
    engine::when_settled(ctx, Ok(promise.into()), move |ctx, r| {
        let module = loader(ctx).pending.borrow_mut().remove(&id);
        match (r, module) {
            (Ok(_), Some(m)) => {
                let ns = m.namespace(ctx);
                done(ctx, Ok(ns.into()))
            }
            (Ok(v), None) => done(ctx, Ok(v)),
            (Err(e), _) => done(ctx, Err(e)),
        }
    });
}

/// `require(specifier)` from `parent` (a filename) or the cwd.
pub(crate) fn require(ctx: &mut Context, specifier: &str, parent: Option<&Path>) -> JsResult<JsValue> {
    let l = loader(ctx);
    let base = referrer_dir(ctx, parent);
    let resolved = l.resolve(ctx, specifier, &base, Kind::Require).map_err(|e| error::to_boa(&e, ctx))?;
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
    match l.resolve(ctx, &spec, &base, kind).map_err(|e| error::to_boa(&e, ctx))? {
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
        return Err(error::to_boa(&err, ctx));
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
    let out = transpile::transpile(&code, Path::new(&name), SourceLang::from_path(Path::new(&name))).map_err(|e| error::to_boa(&e, ctx))?;
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
