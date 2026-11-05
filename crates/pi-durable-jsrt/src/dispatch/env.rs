//! `@earendil-works/pi-durable/env` and `/env/node` (`js/env/*.js`): the decoders, the line
//! scanner and `NodeExecutionEnv` over `pi_durable::env::NativeFileSystem` (`fs.*`, `reader.*`,
//! `watch.*`, `env.*` methods). Re-hosts `pi-durable/examples/env_stock_bridge.rs` in process:
//! policy, reader state, decoding, shell processes and watchers run in Rust; opened-file I/O and
//! spill streams go through the runtime's own `node:fs` (as upstream's `NodeExecutionEnv` does),
//! so a test's `vi.spyOn(FileHandle.prototype, …)` or `vi.mock("node:fs")` reaches them.
//!
//! [`js_env`] is the other direction: any JS `ExecutionEnv` object (a subclass, a test double)
//! as a native `ExecutionEnv`, for native tools and services.

mod host;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use pi_chord::context::BACKGROUND_CONTEXT;
use pi_durable::env::decode::{RangeDecoder, StreamDecoder};
use pi_durable::env::*;
use pi_durable::{Context, JsonString};
use pi_jsrt::JsHandle;

use crate::errors::{ErrorClass, HostError, HostResult};
use crate::host::{Args, Dispatch, Methods, call_async, call_sync};
use crate::value::HostValue;

pub use host::{JsEnv, js_env};

/// The environment method group.
pub struct EnvDispatch;

// ---------------------------------------------------------------------------------------------
// Shared conversions (also used by `tools`, `images` and `testing`).

/// A serializable native value as strict JSON (`pi_durable`'s wire encoding).
pub fn encode<T: serde::Serialize + ?Sized>(value: &T) -> pi_json::strict::Value {
    pi_chord::json::to_value(value).expect("native records are JSON serializable")
}

/// Strict JSON as a deserializable native value (`pi_durable`'s wire decoding).
pub fn decode<T: serde::de::DeserializeOwned>(value: &pi_json::strict::Value) -> Result<T, String> {
    serde_json::from_str(&value.to_string()).map_err(|e| e.to_string())
}

/// Upstream's `FileErrorCode` string.
pub fn file_code(code: FileErrorCode) -> &'static str {
    code.as_str()
}

/// The `FileErrorCode` for an upstream code string (`unknown` for anything else).
pub fn parse_file_code(code: Option<&HostValue>) -> FileErrorCode {
    code.and_then(HostValue::as_string)
        .map(|c| FileErrorCode::from_name(&c.to_string_lossy()))
        .unwrap_or(FileErrorCode::Unknown)
}

/// Upstream's `ExecutionErrorCode` string.
pub fn execution_code(code: ExecutionErrorCode) -> &'static str {
    code.as_str()
}

/// The `ExecutionErrorCode` for an upstream code string (`unknown` for anything else).
pub fn parse_execution_code(code: Option<&HostValue>) -> ExecutionErrorCode {
    code.and_then(HostValue::as_string)
        .map(|c| ExecutionErrorCode::from_name(&c.to_string_lossy()))
        .unwrap_or(ExecutionErrorCode::Unknown)
}

/// A path as a JS string.
pub fn path_value(path: &Path) -> HostValue {
    HostValue::string(&path.to_string_lossy())
}

/// A JS `FileError` instance.
pub fn file_error(error: &FileError) -> HostError {
    let mut e = HostError::new(ErrorClass::FileError, error.to_string())
        .with_prop("code", file_code(error.code()));
    if let Some(path) = error.path() {
        e = e.with_prop("path", path_value(path));
    }
    e
}

/// A JS `ExecutionError` instance.
pub fn execution_error(error: &ExecutionError) -> HostError {
    let mut e = HostError::new(ErrorClass::ExecutionError, error.to_string())
        .with_prop("code", execution_code(error.code()));
    if let Some(path) = error.spill_path() {
        e = e.with_prop("spillPath", path_value(path));
    }
    e
}

/// A plain `{ code, message, path }` (as the glue normalizes JS `FileError`s) as a `FileError`.
pub fn file_error_from(value: &HostValue) -> FileError {
    FileError::new(
        parse_file_code(value.get("code")),
        value
            .get("message")
            .and_then(HostValue::as_string)
            .map(JsonString::to_string_lossy)
            .unwrap_or_else(|| "Host file operation failed".into()),
        value
            .get("path")
            .and_then(HostValue::as_string)
            .map(|p| PathBuf::from(p.to_string_lossy())),
    )
}

/// A plain `{ code, message, spillPath }` as an `ExecutionError`.
pub fn execution_error_from(value: &HostValue) -> ExecutionError {
    let error = ExecutionError::new(
        parse_execution_code(value.get("code")),
        value
            .get("message")
            .and_then(HostValue::as_string)
            .map(JsonString::to_string_lossy)
            .unwrap_or_else(|| "Environment execution failed".into()),
    );
    match value.get("spillPath").and_then(HostValue::as_string) {
        Some(path) => error.with_spill_path(path.to_string_lossy().into()),
        None => error,
    }
}

/// `FileInfo` as upstream's object.
pub fn info_value(info: &FileInfo) -> HostValue {
    HostValue::object([
        ("name", HostValue::string(&info.name.to_string_lossy())),
        ("path", path_value(&info.path)),
        (
            "kind",
            HostValue::from(match info.kind {
                FileKind::File => "file",
                FileKind::Directory => "directory",
                FileKind::Symlink => "symlink",
            }),
        ),
        ("size", HostValue::from(info.size as f64)),
        ("mtimeMs", HostValue::from(info.mtime_ms)),
    ])
}

/// Upstream's file-info object as `FileInfo`.
pub fn parse_info(value: &HostValue) -> FileResult<FileInfo> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(HostValue::as_string)
            .map(JsonString::to_string_lossy)
    };
    let invalid = |message: &str| FileError::new(FileErrorCode::Invalid, message, None);
    let path = text("path").ok_or_else(|| invalid("File info without a path"))?;
    let kind = match text("kind").as_deref() {
        Some("file") => FileKind::File,
        Some("directory") => FileKind::Directory,
        Some("symlink") => FileKind::Symlink,
        _ => {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "Unsupported file type",
                Some(path.into()),
            ));
        }
    };
    Ok(FileInfo {
        name: text("name").unwrap_or_default().into(),
        path: path.into(),
        kind,
        size: value.get("size").and_then(HostValue::as_f64).unwrap_or(0.0) as u64,
        mtime_ms: value
            .get("mtimeMs")
            .and_then(HostValue::as_f64)
            .unwrap_or(0.0),
    })
}

/// `LineScan` as upstream's object.
pub fn scan_value(scan: &LineScan) -> HostValue {
    HostValue::object([
        ("newlines", HostValue::from(scan.newlines as f64)),
        ("start", HostValue::from(scan.start as f64)),
        ("end", HostValue::from(scan.end as f64)),
        ("firstLineEnd", HostValue::from(scan.first_line_end as f64)),
        (
            "lastLineStart",
            HostValue::from(scan.last_line_start as f64),
        ),
        ("selectedBytes", HostValue::from(scan.selected_bytes as f64)),
        (
            "firstLineBytes",
            HostValue::from(scan.first_line_bytes as f64),
        ),
    ])
}

/// Upstream's line-scan object as `LineScan`.
pub fn parse_scan(value: &HostValue) -> LineScan {
    let n = |key: &str| value.get(key).and_then(HostValue::as_f64).unwrap_or(0.0) as u64;
    LineScan {
        newlines: n("newlines"),
        start: n("start"),
        end: n("end"),
        first_line_end: n("firstLineEnd"),
        last_line_start: n("lastLineStart"),
        selected_bytes: n("selectedBytes"),
        first_line_bytes: n("firstLineBytes"),
    }
}

/// A number argument as JS would see it after `Number(x)` for numbers only: anything else is
/// `NaN`, so native validation rejects it like upstream's `Number.isFinite` checks.
pub fn number(value: &HostValue) -> f64 {
    value.as_f64().unwrap_or(f64::NAN)
}

/// `Some(number)` unless `undefined`/`null`.
pub fn optional_number(value: Option<&HostValue>) -> Option<f64> {
    match value {
        None | Some(HostValue::Undefined) | Some(HostValue::Null) => None,
        Some(v) => Some(number(v)),
    }
}

fn text_arg(args: &Args, i: usize) -> String {
    match args.get(i) {
        HostValue::String(s) => s.to_string_lossy(),
        _ => String::new(),
    }
}

fn path_arg(args: &Args, i: usize) -> PathBuf {
    PathBuf::from(text_arg(args, i))
}

/// The native context for a JS call: the glue passes `nativeContext(context)`
/// (`pi-durable:internal/chord/context`), whose cancellation follows the JS context's.
pub fn native_context(value: &HostValue) -> Context {
    match value.as_native::<Context>() {
        Some(context) => (*context).clone(),
        None => BACKGROUND_CONTEXT.clone(),
    }
}

/// Run a blocking call into JS from a Tokio worker without stalling the runtime.
pub fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// [`call_sync`] from native code that may run on a Tokio worker or, re-entrantly, on the JS
/// thread (inside a sync method).
pub fn call_js(
    target: &JsHandle,
    method: Option<&str>,
    args: Vec<HostValue>,
) -> HostResult<HostValue> {
    if target.runtime().is_js_thread() {
        call_sync(target, method, args)
    } else {
        blocking(|| call_sync(target, method, args))
    }
}

/// A port function's plain result: `{ ok: true, value }` or `{ ok: false, error: { … } }`.
fn port_file(result: HostResult<HostValue>) -> FileResult<HostValue> {
    let value = result
        .map_err(|e| FileError::new(FileErrorCode::Unknown, e.message().to_string_lossy(), None))?;
    if value.get("ok").and_then(HostValue::as_bool) == Some(true) {
        Ok(value.get("value").cloned().unwrap_or_default())
    } else {
        Err(file_error_from(
            value.get("error").unwrap_or(&HostValue::Undefined),
        ))
    }
}

fn host_execution_error(error: FileError) -> ExecutionError {
    ExecutionError::new(ExecutionErrorCode::Unknown, error.to_string()).with_cause(Arc::new(error))
}

// ---------------------------------------------------------------------------------------------
// Opened files and spill streams through the runtime's `node:fs` (the glue's port object).

struct PortFile {
    port: JsHandle,
    file: JsHandle,
}

impl PortFile {
    fn call(&self, method: &'static str, mut args: Vec<HostValue>) -> FileFuture<HostValue> {
        let port = self.port.clone();
        args.insert(0, HostValue::Handle(self.file.clone()));
        Box::pin(async move { port_file(call_async(&port, Some(method), args).await) })
    }
}

impl FileHandle for PortFile {
    fn info(&self, _context: &Context) -> FileFuture<FileInfo> {
        let op = self.call("fileInfo", vec![]);
        Box::pin(async move { parse_info(&op.await?) })
    }
    fn read(&self, offset: u64, length: usize, _context: &Context) -> FileFuture<Vec<u8>> {
        let op = self.call(
            "fileRead",
            vec![
                HostValue::from(offset as f64),
                HostValue::from(length as f64),
            ],
        );
        Box::pin(async move {
            match op.await? {
                HostValue::Bytes(bytes) => Ok(bytes),
                _ => Err(FileError::new(
                    FileErrorCode::Invalid,
                    "read returned no bytes",
                    None,
                )),
            }
        })
    }
    fn sync(&self, _context: &Context) -> FileFuture<()> {
        let op = self.call("fileSync", vec![]);
        Box::pin(async move { op.await.map(drop) })
    }
    fn close(&self, _context: &Context) -> FileFuture<()> {
        let op = self.call("fileClose", vec![]);
        Box::pin(async move { op.await.map(drop) })
    }
}

fn file_opener(port: JsHandle) -> FileOpener {
    Arc::new(
        move |path: PathBuf, options: OpenFileOptions, _context: Context| {
            let port = port.clone();
            Box::pin(async move {
                let opened = port_file(
                    call_async(
                        &port,
                        Some("fileOpen"),
                        vec![
                            path_value(&path),
                            HostValue::from(options.write),
                            HostValue::from(options.regular),
                            HostValue::from(options.no_follow),
                        ],
                    )
                    .await,
                )?;
                let file = opened.as_handle().cloned().ok_or_else(|| {
                    FileError::new(FileErrorCode::Invalid, "open returned no file", Some(path))
                })?;
                Ok(Arc::new(PortFile { port, file }) as Arc<dyn FileHandle>)
            })
        },
    )
}

struct PortSpill {
    port: JsHandle,
    stream: JsHandle,
}

impl PortSpill {
    fn call(&self, method: &'static str, mut args: Vec<HostValue>) -> ExecutionFuture<HostValue> {
        let port = self.port.clone();
        args.insert(0, HostValue::Handle(self.stream.clone()));
        Box::pin(async move {
            port_file(call_async(&port, Some(method), args).await).map_err(host_execution_error)
        })
    }
}

impl ShellSpillWriter for PortSpill {
    fn write(&self, bytes: Vec<u8>) -> ExecutionFuture<bool> {
        let op = self.call("spillWrite", vec![HostValue::Bytes(bytes)]);
        Box::pin(async move { Ok(op.await?.as_bool().unwrap_or(false)) })
    }
    fn drain(&self) -> ExecutionFuture<()> {
        let op = self.call("spillDrain", vec![]);
        Box::pin(async move { op.await.map(drop) })
    }
    fn finish(&self) -> ExecutionFuture<()> {
        let op = self.call("spillFinish", vec![]);
        Box::pin(async move { op.await.map(drop) })
    }
    fn settled(&self) -> ExecutionFuture<()> {
        let op = self.call("spillSettled", vec![]);
        Box::pin(async move { op.await.map(drop) })
    }
}

fn spill_opener(port: JsHandle) -> ShellSpillOpener {
    Arc::new(move |path: PathBuf| {
        let port = port.clone();
        Box::pin(async move {
            let stream =
                port_file(call_async(&port, Some("spillOpen"), vec![path_value(&path)]).await)
                    .map_err(host_execution_error)?;
            let stream = stream.as_handle().cloned().ok_or_else(|| {
                ExecutionError::new(ExecutionErrorCode::Unknown, "spill open returned no stream")
            })?;
            Ok(Arc::new(PortSpill { port, stream }) as Arc<dyn ShellSpillWriter>)
        })
    })
}

// ---------------------------------------------------------------------------------------------
// `NodeExecutionEnv` state.

/// One JS `NodeExecutionEnv`'s native environment.
///
/// Text line readers and `flushFile` open their file through the runtime's `node:fs/promises`
/// (`hosted`), like upstream. Binary readers open natively (`fs`): they need `O_NONBLOCK` (a FIFO
/// must not wait for a writer) and `O_NOFOLLOW`, which pi-jsrt's `fs.open` cannot express.
pub struct NodeEnv {
    /// The environment: native files, shell and watchers; spills through the port.
    pub fs: NativeFileSystem,
    hosted: NativeFileSystem,
    shell: NativeShellOptions,
    port: JsHandle,
}

enum Reader {
    Text(Arc<dyn TextLineReader>),
    Binary(Arc<dyn BinaryReader>),
    Directory(Arc<dyn DirReader>),
}

struct Watched {
    watcher: Arc<dyn FileWatcher>,
}

fn strings(value: &HostValue) -> Vec<OsString> {
    match value {
        HostValue::Array(items) => items
            .iter()
            .filter_map(HostValue::as_string)
            .map(|s| s.to_string_lossy().into())
            .collect(),
        _ => Vec::new(),
    }
}

fn watch_targets(value: &HostValue) -> Vec<WatchTarget> {
    let HostValue::Array(items) = value else {
        return Vec::new();
    };
    items
        .iter()
        .map(|target| {
            let exclude = target.get("exclude");
            WatchTarget {
                path: target
                    .get("path")
                    .and_then(HostValue::as_string)
                    .map(|p| PathBuf::from(p.to_string_lossy()))
                    .unwrap_or_default(),
                recursive: target.get("recursive").and_then(HostValue::as_bool) == Some(true),
                exclude: WatchExclude {
                    hidden: exclude
                        .and_then(|e| e.get("hidden"))
                        .and_then(HostValue::as_bool)
                        == Some(true),
                    names: exclude
                        .and_then(|e| e.get("names"))
                        .map(strings)
                        .unwrap_or_default(),
                },
            }
        })
        .collect()
}

/// A native watch change as upstream's object (`error` as a plain `{ code, message, path }`
/// the glue turns into a `FileError`).
pub fn change_value(change: &WatchChange) -> HostValue {
    match change {
        WatchChange::Paths(paths) => HostValue::object([(
            "paths",
            HostValue::Array(paths.iter().map(|p| path_value(p)).collect()),
        )]),
        WatchChange::Overflow => HostValue::object([("overflow", HostValue::Bool(true))]),
        WatchChange::Error(error) => HostValue::object([(
            "error",
            HostValue::object([
                ("code", HostValue::from(file_code(error.code()))),
                ("message", HostValue::string(&error.to_string())),
                (
                    "path",
                    error.path().map(path_value).unwrap_or(HostValue::Undefined),
                ),
            ]),
        )]),
    }
}

fn shell_host(value: &HostValue) -> ShellHost {
    ShellHost {
        cwd: value
            .get("cwd")
            .and_then(HostValue::as_string)
            .map(|c| PathBuf::from(c.to_string_lossy()))
            .unwrap_or_default(),
        platform: if value
            .get("platform")
            .and_then(HostValue::as_string)
            .is_some_and(|p| p.equals_str("win32"))
        {
            ShellPlatform::Windows
        } else {
            ShellPlatform::Unix
        },
        environment: match value.get("env") {
            Some(HostValue::Object(entries)) => entries
                .iter()
                .filter_map(|(k, v)| {
                    v.as_string()
                        .map(|v| (k.to_string_lossy().into(), v.to_string_lossy().into()))
                })
                .collect(),
            _ => BTreeMap::new(),
        },
    }
}

fn shell_command(value: &HostValue) -> ExecutionResult<ShellCommand> {
    match value {
        HostValue::String(s) => Ok(ShellCommand::Script(s.clone())),
        HostValue::Array(items) => items
            .iter()
            .map(|item| {
                item.as_string().cloned().ok_or_else(|| {
                    ExecutionError::new(
                        ExecutionErrorCode::SpawnError,
                        "Command arguments must be strings",
                    )
                })
            })
            .collect::<ExecutionResult<Vec<_>>>()
            .map(ShellCommand::Argv),
        _ => Err(ExecutionError::new(
            ExecutionErrorCode::SpawnError,
            "Command must be a string or an array of strings",
        )),
    }
}

/// Upstream `ShellExecOptions` (minus `onOutput`) as native options.
pub fn exec_options(value: &HostValue) -> HostResult<ShellExecOptions> {
    let get = |key: &str| {
        value
            .get(key)
            .filter(|v| !matches!(v, HostValue::Undefined | HostValue::Null))
    };
    let env = match get("env") {
        Some(env) => {
            pi_durable::JsonObject::try_from_value(env.to_strict_omitting_undefined()?).ok()
        }
        None => None,
    };
    Ok(ShellExecOptions {
        cwd: get("cwd")
            .and_then(HostValue::as_string)
            .map(|c| PathBuf::from(c.to_string_lossy())),
        env,
        inherit_env: get("inheritEnv").and_then(HostValue::as_bool),
        timeout: get("timeout").map(number),
        on_output: None,
        spill: get("spill").map(|spill| ShellSpillOptions {
            after_bytes: spill.get("afterBytes").map(number).unwrap_or(f64::NAN),
            after_lines: spill.get("afterLines").map(number).unwrap_or(f64::NAN),
        }),
        window: get("window").map(|window| {
            let n = |key: &str| window.get(key).map(number).unwrap_or(f64::NAN);
            ShellOutputWindow {
                max_bytes: n("maxBytes").max(0.0) as usize,
                max_lines: n("maxLines").max(0.0) as usize,
                min_interval_ms: n("minIntervalMs"),
                bytes_per_second: n("bytesPerSecond"),
            }
        }),
    })
}

/// Upstream's `ShellOutputInfo`.
pub fn output_info_value(info: &ShellOutputInfo) -> HostValue {
    let stream = HostValue::from(match info.stream {
        ShellStream::Stdout => "stdout",
        ShellStream::Stderr => "stderr",
    });
    match info.skipped {
        None => HostValue::object([("stream", stream)]),
        Some(skip) => HostValue::object([
            ("stream", stream),
            (
                "skipped",
                HostValue::object([
                    ("bytes", HostValue::from(skip.bytes as f64)),
                    ("newlines", HostValue::from(skip.newlines as f64)),
                    ("endsWithNewline", HostValue::from(skip.ends_with_newline)),
                ]),
            ),
        ]),
    }
}

/// Upstream's `ShellOutputInfo` as native.
pub fn parse_output_info(value: &HostValue) -> ShellOutputInfo {
    let stream = if value
        .get("stream")
        .and_then(HostValue::as_string)
        .is_some_and(|s| s.equals_str("stderr"))
    {
        ShellStream::Stderr
    } else {
        ShellStream::Stdout
    };
    let skipped = value
        .get("skipped")
        .filter(|v| matches!(v, HostValue::Object(_)))
        .map(|skip| {
            let n = |key: &str| {
                skip.get(key)
                    .and_then(HostValue::as_f64)
                    .unwrap_or(0.0)
                    .max(0.0) as usize
            };
            ShellOutputSkip {
                bytes: n("bytes"),
                newlines: n("newlines"),
                ends_with_newline: skip.get("endsWithNewline").and_then(HostValue::as_bool)
                    == Some(true),
            }
        });
    ShellOutputInfo { stream, skipped }
}

/// A JS `onOutput(text, info)` function (the glue closes over the caller's context) as native.
pub fn output_callback(handle: JsHandle) -> ShellOutput {
    Arc::new(
        move |text: JsonString, _context: Context, info: ShellOutputInfo| {
            let args = vec![HostValue::String(text), output_info_value(&info)];
            call_js(&handle, None, args).map(drop).map_err(|e| {
                ExecutionError::new(
                    ExecutionErrorCode::CallbackError,
                    e.message().to_string_lossy(),
                )
            })
        },
    )
}

fn exec_result_value(result: &ShellExecResult) -> HostValue {
    match &result.spill_path {
        Some(path) => HostValue::object([
            ("exitCode", HostValue::from(result.exit_code as f64)),
            ("spillPath", path_value(path)),
        ]),
        None => HostValue::object([("exitCode", HostValue::from(result.exit_code as f64))]),
    }
}

fn env_state(args: &Args) -> HostResult<Arc<NodeEnv>> {
    args.native::<NodeEnv>(0)
}

/// Register an async `fs.*` method: arguments `(env, controller, …rest)`.
fn fs_method<F, Fut>(m: &mut Methods, name: &str, f: F)
where
    F: Fn(Arc<NodeEnv>, Args, Context) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = FileResult<HostValue>> + Send + 'static,
{
    m.asynchronous(name, move |_, args| {
        let f = f.clone();
        async move {
            let state = env_state(&args)?;
            let context = native_context(args.get(1));
            f(state, args, context).await.map_err(|e| file_error(&e))
        }
    });
}

fn void(r: FileResult<()>) -> FileResult<HostValue> {
    r.map(|()| HostValue::Undefined)
}

fn reader_state(args: &Args) -> HostResult<Arc<Reader>> {
    args.native::<Reader>(0)
}

impl Dispatch for EnvDispatch {
    fn register(&self, m: &mut Methods) {
        host::register(m);
        // decode.ts / line-scan.ts
        m.sync("env.rangeDecoder", |_, _| {
            Ok(HostValue::native(Mutex::new(RangeDecoder::default())))
        });
        m.sync("env.rangeDecode", |_, args| {
            let decoder = args.native::<Mutex<RangeDecoder>>(0)?;
            let bytes = args.get(1).as_bytes().unwrap_or_default().to_vec();
            let flush = args.get(2).as_bool() != Some(true);
            Ok(HostValue::string(&decoder.lock().decode(&bytes, flush)))
        });
        m.sync("env.streamDecoder", |_, _| {
            Ok(HostValue::native(Mutex::new(StreamDecoder::default())))
        });
        m.sync("env.streamDecode", |_, args| {
            let decoder = args.native::<Mutex<StreamDecoder>>(0)?;
            let text = match args.get(1) {
                HostValue::Undefined => decoder.lock().decode(None),
                value => decoder
                    .lock()
                    .decode(Some(value.as_bytes().unwrap_or_default())),
            };
            Ok(HostValue::string(&text))
        });
        m.sync("env.lineScanner", |_, args| {
            let start = args.get(0).as_f64().unwrap_or(f64::NAN);
            let end = match args.get(1) {
                HostValue::Undefined => None,
                v => Some(v.as_f64().unwrap_or(f64::NAN)).filter(|e| *e != f64::INFINITY),
            };
            LineScanner::new(start, end)
                .map(|s| HostValue::native(Mutex::new(s)))
                .map_err(|e| HostError::new(ErrorClass::RangeError, e))
        });
        m.sync("env.lineScannerPush", |_, args| {
            let scanner = args.native::<Mutex<LineScanner>>(0)?;
            scanner
                .lock()
                .push(args.get(1).as_bytes().unwrap_or_default());
            Ok(HostValue::Undefined)
        });
        m.sync("env.lineScannerFinish", |_, args| {
            let scanner = args.native::<Mutex<LineScanner>>(0)?;
            let scan = scanner.lock().finish();
            Ok(scan_value(&scan))
        });

        // node.ts: construction and the synchronous cwd setter.
        m.sync("fs.new", |_, args| {
            let options = args.get(0);
            let port = args.handle(1)?;
            let text = |key: &str| {
                options
                    .get(key)
                    .and_then(HostValue::as_string)
                    .map(JsonString::to_string_lossy)
            };
            let cwd = text("cwd").unwrap_or_default();
            let environment = match options.get("shellEnv") {
                Some(HostValue::Object(entries)) => entries
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.to_string_lossy().into(),
                            v.as_string().map(|v| v.to_string_lossy().into()),
                        )
                    })
                    .collect(),
                _ => BTreeMap::new(),
            };
            let shell = NativeShellOptions {
                shell_path: text("shellPath").map(PathBuf::from),
                environment,
                spill_opener: Some(spill_opener(port.clone())),
                ..Default::default()
            };
            let watch = options.get("watch");
            let field = |key: &str| optional_number(watch.and_then(|w| w.get(key)));
            let mode = match watch.and_then(|w| w.get("mode")) {
                None | Some(HostValue::Undefined) | Some(HostValue::Null) => None,
                Some(HostValue::String(s)) if s.equals_str("native") => Some(WatchMode::Native),
                Some(HostValue::String(s)) if s.equals_str("polling") => Some(WatchMode::Polling),
                Some(_) => {
                    return Err(HostError::type_error(
                        "watch.mode must be \"native\" or \"polling\"",
                    ));
                }
            };
            let watch = NativeWatchOptions {
                mode,
                poll_interval_ms: field("pollIntervalMs"),
                max_directories: field("maxDirectories"),
                ..Default::default()
            };
            let fs = NativeFileSystem::new(&cwd)
                .map_err(|e| file_error(&e))?
                .with_shell_options(shell.clone())
                .with_watch_options(watch);
            let hosted = NativeFileSystem::with_file_opener(&cwd, file_opener(port.clone()))
                .map_err(|e| file_error(&e))?;
            let id = HostValue::string(fs.id());
            Ok(HostValue::object([
                (
                    "native",
                    HostValue::native(NodeEnv {
                        fs,
                        hosted,
                        shell,
                        port,
                    }),
                ),
                ("id", id),
            ]))
        });
        m.sync("fs.setCwd", |_, args| {
            let state = env_state(&args)?;
            state.fs.set_cwd(path_arg(&args, 1));
            state.hosted.set_cwd(path_arg(&args, 1));
            Ok(HostValue::Undefined)
        });

        fs_method(m, "fs.absolute", |s, a, c| async move {
            Ok(path_value(&s.fs.absolute_path(&path_arg(&a, 2), &c).await?))
        });
        fs_method(m, "fs.join", |s, a, c| async move {
            let parts: Vec<PathBuf> = match a.get(2) {
                HostValue::Array(items) => items
                    .iter()
                    .map(|p| {
                        PathBuf::from(
                            p.as_string()
                                .map(JsonString::to_string_lossy)
                                .unwrap_or_default(),
                        )
                    })
                    .collect(),
                _ => Vec::new(),
            };
            Ok(path_value(&s.fs.join_path(&parts, &c).await?))
        });
        fs_method(m, "fs.readText", |s, a, c| async move {
            Ok(HostValue::string(
                &s.fs.read_text_file(&path_arg(&a, 2), &c).await?,
            ))
        });
        fs_method(m, "fs.readLines", |s, a, c| async move {
            let max = optional_number(Some(a.get(3)));
            let lines = s.fs.read_text_lines(&path_arg(&a, 2), max, &c).await?;
            Ok(HostValue::Array(
                lines.iter().map(|l| HostValue::string(l)).collect(),
            ))
        });
        fs_method(m, "fs.readBinary", |s, a, c| async move {
            Ok(HostValue::Bytes(
                s.fs.read_binary_file(&path_arg(&a, 2), &c).await?,
            ))
        });
        fs_method(m, "fs.write", |s, a, c| async move {
            let path = path_arg(&a, 2);
            void(match a.get(3) {
                HostValue::String(text) => s.fs.write_text_file(&path, text, &c).await,
                value => {
                    s.fs.write_file(&path, value.as_bytes().unwrap_or_default(), &c)
                        .await
                }
            })
        });
        fs_method(m, "fs.append", |s, a, c| async move {
            let path = path_arg(&a, 2);
            let bytes = match a.get(3) {
                HostValue::String(text) => text.to_string_lossy().into_bytes(),
                value => value.as_bytes().unwrap_or_default().to_vec(),
            };
            void(s.fs.append_file(&path, &bytes, &c).await)
        });
        fs_method(m, "fs.truncate", |s, a, c| async move {
            let path = path_arg(&a, 2);
            let size = number(a.get(3));
            if !size.is_finite()
                || size < 0.0
                || size.fract() != 0.0
                || size > pi_durable::MAX_SAFE_INTEGER as f64
            {
                return Err(FileError::new(
                    FileErrorCode::Invalid,
                    "File size must be a non-negative safe integer",
                    Some(s.fs.absolute_path(&path, &c).await?),
                ));
            }
            void(s.fs.truncate_file(&path, size as u64, &c).await)
        });
        fs_method(m, "fs.flush", |s, a, c| async move {
            void(s.hosted.flush_file(&path_arg(&a, 2), &c).await)
        });
        fs_method(m, "fs.rename", |s, a, c| async move {
            void(
                s.fs.rename_file(&path_arg(&a, 2), &path_arg(&a, 3), &c)
                    .await,
            )
        });
        fs_method(m, "fs.info", |s, a, c| async move {
            Ok(info_value(&s.fs.file_info(&path_arg(&a, 2), &c).await?))
        });
        fs_method(m, "fs.list", |s, a, c| async move {
            let list = s.fs.list_dir(&path_arg(&a, 2), &c).await?;
            Ok(HostValue::Array(list.iter().map(info_value).collect()))
        });
        fs_method(m, "fs.canonical", |s, a, c| async move {
            Ok(path_value(
                &s.fs.canonical_path(&path_arg(&a, 2), &c).await?,
            ))
        });
        fs_method(m, "fs.exists", |s, a, c| async move {
            Ok(HostValue::from(s.fs.exists(&path_arg(&a, 2), &c).await?))
        });
        fs_method(m, "fs.mkdir", |s, a, c| async move {
            let recursive = a.get(3).as_bool().unwrap_or(true);
            void(s.fs.create_dir(&path_arg(&a, 2), recursive, &c).await)
        });
        fs_method(m, "fs.remove", |s, a, c| async move {
            let recursive = a.get(3).as_bool() == Some(true);
            let force = a.get(4).as_bool() == Some(true);
            void(s.fs.remove(&path_arg(&a, 2), recursive, force, &c).await)
        });
        fs_method(m, "fs.tempDir", |s, a, c| async move {
            let prefix = a.get(2).as_string().map(JsonString::to_string_lossy);
            Ok(path_value(
                &s.fs.create_temp_dir(prefix.as_deref(), &c).await?,
            ))
        });
        fs_method(m, "fs.tempFile", |s, a, c| async move {
            let prefix = a.get(2).as_string().map(JsonString::to_string_lossy);
            let suffix = a.get(3).as_string().map(JsonString::to_string_lossy);
            Ok(path_value(
                &s.fs
                    .create_temp_file(prefix.as_deref(), suffix.as_deref(), &c)
                    .await?,
            ))
        });
        fs_method(m, "fs.openText", |s, a, c| async move {
            let reader = s.hosted.open_text_line_reader(&path_arg(&a, 2), &c).await?;
            Ok(HostValue::native(Reader::Text(reader)))
        });
        fs_method(m, "fs.openBinary", |s, a, c| async move {
            let no_follow = a.get(3).as_bool() == Some(true);
            let reader =
                s.fs.open_binary_reader(&path_arg(&a, 2), no_follow, &c)
                    .await?;
            Ok(HostValue::native(Reader::Binary(reader)))
        });
        fs_method(m, "fs.openDir", |s, a, c| async move {
            let reader = s.fs.open_dir_reader(&path_arg(&a, 2), &c).await?;
            Ok(HostValue::native(Reader::Directory(reader)))
        });
        fs_method(m, "fs.watch", |s, a, c| async move {
            let targets = watch_targets(a.get(2));
            let deliver = a.handle(3).map_err(|e| {
                FileError::new(FileErrorCode::Invalid, e.message().to_string_lossy(), None)
            })?;
            let callback: WatchCallback = Arc::new(move |change: WatchChange| {
                let value = change_value(&change);
                let _ = call_js(&deliver, None, vec![value]);
            });
            let watcher = s.fs.watch(&targets, callback, &c).await?;
            let mode = watcher.mode();
            Ok(HostValue::object([
                ("watcher", HostValue::native(Watched { watcher })),
                (
                    "mode",
                    HostValue::from(match mode {
                        WatchMode::Native => "native",
                        WatchMode::Polling => "polling",
                    }),
                ),
            ]))
        });
        m.asynchronous("fs.cleanup", |_, args| async move {
            let state = env_state(&args)?;
            let context = native_context(args.get(1));
            state
                .fs
                .cleanup(&context)
                .await
                .map_err(|e| execution_error(&e))?;
            Ok(HostValue::Undefined)
        });
        // exec(env, controller, self, command, options, onOutput, host)
        m.asynchronous("fs.exec", |_, args| async move {
            let state = env_state(&args)?;
            let context = native_context(args.get(1));
            let this = args.handle(2)?;
            let command = shell_command(args.get(3)).map_err(|e| execution_error(&e))?;
            let mut options = exec_options(args.get(4))?;
            if let HostValue::Handle(on_output) = args.get(5) {
                options.on_output = Some(output_callback(on_output.clone()));
            }
            let port = state.port.clone();
            let temp_file: ShellTempFile = Arc::new(move |_context: Context| {
                let port = port.clone();
                let this = this.clone();
                Box::pin(async move {
                    let path = port_file(
                        call_async(&port, Some("shellTemp"), vec![HostValue::Handle(this)]).await,
                    )?;
                    Ok(PathBuf::from(
                        path.as_string()
                            .map(JsonString::to_string_lossy)
                            .unwrap_or_default(),
                    ))
                })
            });
            let fs = state
                .fs
                .clone()
                .with_shell_options(NativeShellOptions {
                    temp_file: Some(temp_file),
                    ..state.shell.clone()
                })
                .with_shell_host(shell_host(args.get(6)));
            let result = fs
                .exec(command, options, &context)
                .await
                .map_err(|e| execution_error(&e))?;
            Ok(exec_result_value(&result))
        });

        // Readers: (reader, controller, …rest).
        m.asynchronous("reader.line", |_, args| async move {
            let Reader::Text(reader) = &*reader_state(&args)? else {
                return Err(HostError::type_error("not a text line reader"));
            };
            let context = native_context(args.get(1));
            let line = reader
                .read_line(&context)
                .await
                .map_err(|e| file_error(&e))?;
            Ok(line
                .map(|line| {
                    HostValue::object([
                        ("text", HostValue::string(&line.text)),
                        ("terminated", HostValue::from(line.terminated)),
                    ])
                })
                .unwrap_or(HostValue::Undefined))
        });
        m.asynchronous("reader.info", |_, args| async move {
            let Reader::Binary(reader) = &*reader_state(&args)? else {
                return Err(HostError::type_error("not a binary reader"));
            };
            let context = native_context(args.get(1));
            let info = reader.info(&context).await.map_err(|e| file_error(&e))?;
            Ok(info_value(&info))
        });
        m.asynchronous("reader.read", |_, args| async move {
            let Reader::Binary(reader) = &*reader_state(&args)? else {
                return Err(HostError::type_error("not a binary reader"));
            };
            let context = native_context(args.get(1));
            let bytes = reader
                .read(number(args.get(2)), number(args.get(3)), &context)
                .await
                .map_err(|e| file_error(&e))?;
            Ok(HostValue::Bytes(bytes))
        });
        m.asynchronous("reader.scan", |_, args| async move {
            let Reader::Binary(reader) = &*reader_state(&args)? else {
                return Err(HostError::type_error("not a binary reader"));
            };
            let context = native_context(args.get(1));
            let scan = reader
                .scan_lines(
                    number(args.get(2)),
                    optional_number(Some(args.get(3))),
                    &context,
                )
                .await
                .map_err(|e| file_error(&e))?;
            Ok(scan_value(&scan))
        });
        m.asynchronous("reader.next", |_, args| async move {
            let Reader::Directory(reader) = &*reader_state(&args)? else {
                return Err(HostError::type_error("not a directory reader"));
            };
            let context = native_context(args.get(1));
            let page = reader
                .next(number(args.get(2)), &context)
                .await
                .map_err(|e| file_error(&e))?;
            Ok(HostValue::object([
                (
                    "entries",
                    HostValue::Array(page.entries.iter().map(info_value).collect()),
                ),
                ("done", HostValue::from(page.done)),
            ]))
        });
        m.asynchronous("reader.close", |_, args| async move {
            let context = native_context(args.get(1));
            match &*reader_state(&args)? {
                Reader::Text(r) => r.close(&context).await,
                Reader::Binary(r) => r.close(&context).await,
                Reader::Directory(r) => r.close(&context).await,
            }
            .map_err(|e| file_error(&e))?;
            Ok(HostValue::Undefined)
        });

        m.asynchronous("watch.close", |_, args| async move {
            let watched = args.native::<Watched>(0)?;
            let context = native_context(args.get(1));
            watched
                .watcher
                .close(&context)
                .await
                .map_err(|e| file_error(&e))?;
            Ok(HostValue::Undefined)
        });
    }
}
