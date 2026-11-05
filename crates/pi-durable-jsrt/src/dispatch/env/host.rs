//! A JS `ExecutionEnv` object as a native `ExecutionEnv` ([`JsEnv`]): every call goes to the JS
//! object's own method (so subclasses, instance overrides and test doubles behave as upstream's
//! tools see them), through the glue's port (`js/internal/env/port.js`), which normalizes the
//! returned `Result` and mirrors native cancellation into a JS `AbortSignal`.
//! Re-hosts `pi-durable/examples/support/tool_environment.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use pi_durable::env::*;
use pi_durable::{Context, JsonString};
use pi_jsrt::JsHandle;

use super::{
    call_js, execution_error_from, file_error_from, parse_info, parse_output_info, parse_scan,
    path_value,
};
use crate::errors::{ErrorClass, HostError, HostResult};
use crate::host::{Methods, call_async};
use crate::value::HostValue;

/// Native cancellation mirrored into the JS `AbortController` the port creates for one call.
#[derive(Default)]
struct SignalLink {
    state: Mutex<(bool, Option<JsHandle>)>,
}

impl SignalLink {
    fn abort(&self) -> Option<JsHandle> {
        let mut state = self.state.lock();
        state.0 = true;
        state.1.take()
    }
}

/// The native side of a JS `onOutput` the port creates for a forwarded `exec`.
struct OutputLink(ShellOutput, Context);

/// The native side of a JS `onChange` the port creates for a forwarded `watch`.
struct WatchLink(WatchCallback);

pub(super) fn register(m: &mut Methods) {
    // (link, controller) -> whether the call was already aborted
    m.sync("env.linkSignal", |_, args| {
        let link = args.native::<SignalLink>(0)?;
        let mut state = link.state.lock();
        if state.0 {
            return Ok(HostValue::Bool(true));
        }
        state.1 = Some(args.handle(1)?);
        Ok(HostValue::Bool(false))
    });
    m.sync("env.output", |_, args| {
        let link = args.native::<OutputLink>(0)?;
        let text = args.string(1)?.clone();
        let info = parse_output_info(args.get(2));
        (link.0)(text, link.1.clone(), info).map_err(|e| {
            HostError::new(ErrorClass::ExecutionError, e.to_string())
                .with_prop("code", super::execution_code(e.code()))
        })?;
        Ok(HostValue::Undefined)
    });
    m.sync("env.watchDeliver", |_, args| {
        let link = args.native::<WatchLink>(0)?;
        let change = args.get(1);
        let change = if let Some(HostValue::Array(paths)) = change.get("paths") {
            WatchChange::Paths(
                paths
                    .iter()
                    .filter_map(HostValue::as_string)
                    .map(|p| PathBuf::from(p.to_string_lossy()))
                    .collect(),
            )
        } else if let Some(error) = change.get("error") {
            WatchChange::Error(file_error_from(error))
        } else {
            WatchChange::Overflow
        };
        (link.0)(change);
        Ok(HostValue::Undefined)
    });
}

/// A JS environment as a native `ExecutionEnv`.
pub struct JsEnv {
    port: JsHandle,
    env: JsHandle,
    id: String,
    cwd: PathBuf,
}

/// The native environment for a description made by the glue's `describeEnv(env)`
/// (`{ port, env, id, cwd }`), or `None` for `undefined`.
pub fn js_env(description: &HostValue) -> HostResult<Option<Arc<dyn ExecutionEnv>>> {
    if matches!(description, HostValue::Undefined | HostValue::Null) {
        return Ok(None);
    }
    let handle = |key: &str| {
        description
            .get(key)
            .and_then(HostValue::as_handle)
            .cloned()
            .ok_or_else(|| HostError::type_error(format!("environment description without {key}")))
    };
    let text = |key: &str| {
        description
            .get(key)
            .and_then(HostValue::as_string)
            .map(JsonString::to_string_lossy)
            .unwrap_or_default()
    };
    Ok(Some(Arc::new(JsEnv {
        port: handle("port")?,
        env: handle("env")?,
        id: text("id"),
        cwd: PathBuf::from(text("cwd")),
    })))
}

/// Call `port[name](…args, link)` with the native context's cancellation linked to JS.
async fn linked(
    port: &JsHandle,
    name: &str,
    mut args: Vec<HostValue>,
    context: &Context,
) -> HostResult<HostValue> {
    let signal = context.abort_signal().cloned();
    let link = signal.as_ref().map(|_| Arc::new(SignalLink::default()));
    if let (Some(link), Some(signal)) = (&link, &signal)
        && signal.aborted()
    {
        link.abort();
    }
    args.push(
        link.clone()
            .map(|l| HostValue::Native(crate::value::NativeRef(l)))
            .unwrap_or_default(),
    );
    let call = call_async(port, Some(name), args);
    let (Some(signal), Some(link)) = (signal, link) else {
        return call.await;
    };
    tokio::pin!(call);
    tokio::select! {
        result = &mut call => result,
        _ = signal.cancelled() => {
            if let Some(controller) = link.abort() {
                let _ = call_async(&controller, Some("abort"), vec![]).await;
            }
            call.await
        }
    }
}

fn file_result(result: HostResult<HostValue>) -> FileResult<HostValue> {
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

fn execution_result(result: HostResult<HostValue>) -> ExecutionResult<HostValue> {
    let value = result.map_err(|e| {
        ExecutionError::new(ExecutionErrorCode::Unknown, e.message().to_string_lossy())
    })?;
    if value.get("ok").and_then(HostValue::as_bool) == Some(true) {
        Ok(value.get("value").cloned().unwrap_or_default())
    } else {
        Err(execution_error_from(
            value.get("error").unwrap_or(&HostValue::Undefined),
        ))
    }
}

fn invalid(message: &str) -> FileError {
    FileError::new(FileErrorCode::Invalid, message, None)
}

fn as_path(value: HostValue) -> FileResult<PathBuf> {
    value
        .as_string()
        .map(|s| PathBuf::from(s.to_string_lossy()))
        .ok_or_else(|| invalid("Environment returned no path"))
}

fn as_bytes(value: HostValue) -> FileResult<Vec<u8>> {
    match value {
        HostValue::Bytes(bytes) => Ok(bytes),
        _ => Err(invalid("Environment returned no bytes")),
    }
}

fn as_handle(value: HostValue) -> FileResult<JsHandle> {
    value
        .as_handle()
        .cloned()
        .ok_or_else(|| invalid("Environment returned no object"))
}

/// One JS object (an environment, reader or watcher) behind the port.
#[derive(Clone)]
struct Target {
    port: JsHandle,
    object: JsHandle,
}

impl Target {
    fn call(
        &self,
        method: &'static str,
        args: Vec<HostValue>,
        raw: bool,
        context: &Context,
    ) -> FileFuture<HostValue> {
        let target = self.clone();
        let context = context.clone();
        Box::pin(async move {
            file_result(
                linked(
                    &target.port,
                    "invoke",
                    vec![
                        HostValue::Handle(target.object.clone()),
                        HostValue::from(method),
                        HostValue::Array(args),
                        HostValue::Bool(raw),
                    ],
                    &context,
                )
                .await,
            )
        })
    }
    fn map<T: Send + 'static>(
        &self,
        method: &'static str,
        args: Vec<HostValue>,
        context: &Context,
        f: impl FnOnce(HostValue) -> FileResult<T> + Send + 'static,
    ) -> FileFuture<T> {
        let op = self.call(method, args, false, context);
        Box::pin(async move { f(op.await?) })
    }
    fn unit(
        &self,
        method: &'static str,
        args: Vec<HostValue>,
        context: &Context,
    ) -> FileFuture<()> {
        self.map(method, args, context, |_| Ok(()))
    }
    fn raw_unit(&self, method: &'static str, context: &Context) -> FileFuture<()> {
        let op = self.call(method, vec![], true, context);
        Box::pin(async move { op.await.map(drop) })
    }
}

impl JsEnv {
    fn target(&self) -> Target {
        Target {
            port: self.port.clone(),
            object: self.env.clone(),
        }
    }
    fn path_call<T: Send + 'static>(
        &self,
        method: &'static str,
        path: &Path,
        context: &Context,
        f: impl FnOnce(HostValue) -> FileResult<T> + Send + 'static,
    ) -> FileFuture<T> {
        self.target()
            .map(method, vec![path_value(path)], context, f)
    }
}

impl FileSystem for JsEnv {
    fn id(&self) -> &str {
        &self.id
    }
    fn cwd(&self) -> PathBuf {
        self.cwd.clone()
    }
    fn absolute_path(&self, path: &Path, context: &Context) -> FileFuture<PathBuf> {
        self.path_call("absolutePath", path, context, as_path)
    }
    fn join_path(&self, parts: &[PathBuf], context: &Context) -> FileFuture<PathBuf> {
        let parts = HostValue::Array(parts.iter().map(|p| path_value(p)).collect());
        self.target().map("joinPath", vec![parts], context, as_path)
    }
    fn read_binary_file(&self, path: &Path, context: &Context) -> FileFuture<Vec<u8>> {
        self.path_call("readBinaryFile", path, context, as_bytes)
    }
    fn read_text_file(&self, path: &Path, context: &Context) -> FileFuture<String> {
        self.path_call("readTextFile", path, context, |v| {
            v.as_string()
                .map(JsonString::to_string_lossy)
                .ok_or_else(|| invalid("Environment returned no text"))
        })
    }
    fn read_text_lines(
        &self,
        path: &Path,
        max_lines: Option<f64>,
        context: &Context,
    ) -> FileFuture<Vec<String>> {
        let options = match max_lines {
            Some(max) => HostValue::object([("maxLines", HostValue::from(max))]),
            None => HostValue::Undefined,
        };
        self.target().map(
            "readTextLines",
            vec![path_value(path), options],
            context,
            |v| match v {
                HostValue::Array(lines) => Ok(lines
                    .iter()
                    .map(|l| {
                        l.as_string()
                            .map(JsonString::to_string_lossy)
                            .unwrap_or_default()
                    })
                    .collect()),
                _ => Err(invalid("Environment returned no lines")),
            },
        )
    }
    fn open_text_line_reader(
        &self,
        path: &Path,
        context: &Context,
    ) -> FileFuture<Arc<dyn TextLineReader>> {
        let port = self.port.clone();
        self.path_call("openTextLineReader", path, context, move |v| {
            Ok(Arc::new(JsReader(Target {
                port,
                object: as_handle(v)?,
            })) as Arc<dyn TextLineReader>)
        })
    }
    fn open_binary_reader(
        &self,
        path: &Path,
        no_follow: bool,
        context: &Context,
    ) -> FileFuture<Arc<dyn BinaryReader>> {
        let port = self.port.clone();
        self.target().map(
            "openBinaryReader",
            vec![
                path_value(path),
                HostValue::object([("noFollow", HostValue::from(no_follow))]),
            ],
            context,
            move |v| {
                Ok(Arc::new(JsReader(Target {
                    port,
                    object: as_handle(v)?,
                })) as Arc<dyn BinaryReader>)
            },
        )
    }
    fn open_dir_reader(&self, path: &Path, context: &Context) -> FileFuture<Arc<dyn DirReader>> {
        let port = self.port.clone();
        self.path_call("openDirReader", path, context, move |v| {
            Ok(Arc::new(JsReader(Target {
                port,
                object: as_handle(v)?,
            })) as Arc<dyn DirReader>)
        })
    }
    fn file_info(&self, path: &Path, context: &Context) -> FileFuture<FileInfo> {
        self.path_call("fileInfo", path, context, |v| parse_info(&v))
    }
    fn canonical_path(&self, path: &Path, context: &Context) -> FileFuture<PathBuf> {
        self.path_call("canonicalPath", path, context, as_path)
    }
    fn exists(&self, path: &Path, context: &Context) -> FileFuture<bool> {
        self.path_call("exists", path, context, |v| {
            v.as_bool()
                .ok_or_else(|| invalid("Environment returned no boolean"))
        })
    }
    fn list_files(
        &self,
        path: &Path,
        options: &ListFilesOptions,
        context: &Context,
    ) -> FileFuture<FileListing> {
        let mut fields = Vec::new();
        if let Some(hidden) = options.hidden {
            fields.push(("hidden", HostValue::from(hidden)));
        }
        if let Some(gitignore) = options.gitignore {
            fields.push(("gitignore", HostValue::from(gitignore)));
        }
        if !options.exclude.is_empty() {
            fields.push((
                "exclude",
                HostValue::Array(
                    options
                        .exclude
                        .iter()
                        .map(|n| HostValue::string(&n.to_string_lossy()))
                        .collect(),
                ),
            ));
        }
        for (key, value) in [
            ("maxEntries", options.max_entries),
            ("maxBytes", options.max_bytes),
            ("timeoutMs", options.timeout_ms),
        ] {
            if let Some(value) = value {
                fields.push((key, HostValue::from(value)));
            }
        }
        self.target().map(
            "listFiles",
            vec![path_value(path), HostValue::object(fields)],
            context,
            |v| {
                let entries = match v.get("entries") {
                    Some(HostValue::Array(entries)) => entries
                        .iter()
                        .map(|e| {
                            let text = |key: &str| {
                                e.get(key)
                                    .and_then(HostValue::as_string)
                                    .map(JsonString::to_string_lossy)
                            };
                            Ok(FileListEntry {
                                path: text("path").unwrap_or_default(),
                                kind: match text("kind").as_deref() {
                                    Some("file") => FileKind::File,
                                    Some("directory") => FileKind::Directory,
                                    Some("symlink") => FileKind::Symlink,
                                    _ => return Err(invalid("Unsupported file type")),
                                },
                            })
                        })
                        .collect::<FileResult<Vec<_>>>()?,
                    _ => return Err(invalid("Environment returned no listing")),
                };
                Ok(FileListing {
                    entries,
                    truncated: v.get("truncated").and_then(HostValue::as_bool) == Some(true),
                })
            },
        )
    }
    fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchCallback,
        context: &Context,
    ) -> FileFuture<Arc<dyn FileWatcher>> {
        let targets = HostValue::Array(
            targets
                .iter()
                .map(|t| {
                    HostValue::object([
                        ("path", path_value(&t.path)),
                        ("recursive", HostValue::from(t.recursive)),
                        (
                            "exclude",
                            HostValue::object([
                                ("hidden", HostValue::from(t.exclude.hidden)),
                                (
                                    "names",
                                    HostValue::Array(
                                        t.exclude
                                            .names
                                            .iter()
                                            .map(|n| HostValue::string(&n.to_string_lossy()))
                                            .collect(),
                                    ),
                                ),
                            ]),
                        ),
                    ])
                })
                .collect(),
        );
        let port = self.port.clone();
        let env = self.env.clone();
        let context = context.clone();
        Box::pin(async move {
            let deliver =
                HostValue::Native(crate::value::NativeRef(Arc::new(WatchLink(on_change))));
            let watcher = as_handle(file_result(
                linked(
                    &port,
                    "watch",
                    vec![HostValue::Handle(env), targets, deliver],
                    &context,
                )
                .await,
            )?)?;
            Ok(Arc::new(JsWatcher(Target {
                port,
                object: watcher,
            })) as Arc<dyn FileWatcher>)
        })
    }
    fn create_temp_dir(&self, prefix: Option<&str>, context: &Context) -> FileFuture<PathBuf> {
        let prefix = prefix.map(HostValue::from).unwrap_or_default();
        self.target()
            .map("createTempDir", vec![prefix], context, as_path)
    }
    fn create_temp_file(
        &self,
        prefix: Option<&str>,
        suffix: Option<&str>,
        context: &Context,
    ) -> FileFuture<PathBuf> {
        let mut fields = Vec::new();
        if let Some(prefix) = prefix {
            fields.push(("prefix", HostValue::from(prefix)));
        }
        if let Some(suffix) = suffix {
            fields.push(("suffix", HostValue::from(suffix)));
        }
        self.target().map(
            "createTempFile",
            vec![HostValue::object(fields)],
            context,
            as_path,
        )
    }
    fn write_file(&self, path: &Path, content: &[u8], context: &Context) -> FileFuture<()> {
        self.target().unit(
            "writeFile",
            vec![path_value(path), HostValue::Bytes(content.to_vec())],
            context,
        )
    }
    fn write_text_file(
        &self,
        path: &Path,
        content: &JsonString,
        context: &Context,
    ) -> FileFuture<()> {
        self.target().unit(
            "writeFile",
            vec![path_value(path), HostValue::String(content.clone())],
            context,
        )
    }
    fn append_file(&self, path: &Path, content: &[u8], context: &Context) -> FileFuture<()> {
        self.target().unit(
            "appendFile",
            vec![path_value(path), HostValue::Bytes(content.to_vec())],
            context,
        )
    }
    fn truncate_file(&self, path: &Path, size: u64, context: &Context) -> FileFuture<()> {
        self.target().unit(
            "truncateFile",
            vec![path_value(path), HostValue::from(size as f64)],
            context,
        )
    }
    fn flush_file(&self, path: &Path, context: &Context) -> FileFuture<()> {
        self.target()
            .unit("flushFile", vec![path_value(path)], context)
    }
    fn rename_file(&self, source: &Path, destination: &Path, context: &Context) -> FileFuture<()> {
        self.target().unit(
            "renameFile",
            vec![path_value(source), path_value(destination)],
            context,
        )
    }
    fn list_dir(&self, path: &Path, context: &Context) -> FileFuture<Vec<FileInfo>> {
        self.path_call("listDir", path, context, |v| match v {
            HostValue::Array(items) => items.iter().map(parse_info).collect(),
            _ => Err(invalid("Environment returned no entries")),
        })
    }
    fn create_dir(&self, path: &Path, recursive: bool, context: &Context) -> FileFuture<()> {
        self.target().unit(
            "createDir",
            vec![
                path_value(path),
                HostValue::object([("recursive", HostValue::from(recursive))]),
            ],
            context,
        )
    }
    fn remove(
        &self,
        path: &Path,
        recursive: bool,
        force: bool,
        context: &Context,
    ) -> FileFuture<()> {
        self.target().unit(
            "remove",
            vec![
                path_value(path),
                HostValue::object([
                    ("recursive", HostValue::from(recursive)),
                    ("force", HostValue::from(force)),
                ]),
            ],
            context,
        )
    }
}

impl Shell for JsEnv {
    fn exec(
        &self,
        command: ShellCommand,
        options: ShellExecOptions,
        context: &Context,
    ) -> ExecutionFuture<ShellExecResult> {
        let command = match command {
            ShellCommand::Script(script) => HostValue::String(script),
            ShellCommand::Argv(argv) => {
                HostValue::Array(argv.into_iter().map(HostValue::String).collect())
            }
        };
        let mut fields = Vec::new();
        if let Some(cwd) = &options.cwd {
            fields.push(("cwd", path_value(cwd)));
        }
        if let Some(env) = &options.env {
            fields.push(("env", HostValue::from_strict(env.value())));
        }
        if let Some(inherit) = options.inherit_env {
            fields.push(("inheritEnv", HostValue::from(inherit)));
        }
        if let Some(timeout) = options.timeout {
            fields.push(("timeout", HostValue::from(timeout)));
        }
        if let Some(spill) = options.spill {
            fields.push((
                "spill",
                HostValue::object([
                    ("afterBytes", HostValue::from(spill.after_bytes)),
                    ("afterLines", HostValue::from(spill.after_lines)),
                ]),
            ));
        }
        if let Some(window) = options.window {
            fields.push((
                "window",
                HostValue::object([
                    ("maxBytes", HostValue::from(window.max_bytes as f64)),
                    ("maxLines", HostValue::from(window.max_lines as f64)),
                    ("minIntervalMs", HostValue::from(window.min_interval_ms)),
                    ("bytesPerSecond", HostValue::from(window.bytes_per_second)),
                ]),
            ));
        }
        let output = options
            .on_output
            .map(|f| {
                HostValue::Native(crate::value::NativeRef(Arc::new(OutputLink(
                    f,
                    context.clone(),
                ))))
            })
            .unwrap_or_default();
        let port = self.port.clone();
        let env = self.env.clone();
        let context = context.clone();
        Box::pin(async move {
            let value = execution_result(
                linked(
                    &port,
                    "exec",
                    vec![
                        HostValue::Handle(env),
                        command,
                        HostValue::object(fields),
                        output,
                    ],
                    &context,
                )
                .await,
            )?;
            Ok(ShellExecResult {
                exit_code: value
                    .get("exitCode")
                    .and_then(HostValue::as_f64)
                    .unwrap_or(0.0) as i32,
                spill_path: value
                    .get("spillPath")
                    .and_then(HostValue::as_string)
                    .map(|p| PathBuf::from(p.to_string_lossy())),
            })
        })
    }
    fn cleanup(&self, context: &Context) -> ExecutionFuture<()> {
        let op = self.target().raw_unit("cleanup", context);
        Box::pin(async move {
            op.await
                .map_err(|e| ExecutionError::new(ExecutionErrorCode::Unknown, e.to_string()))
        })
    }
}

struct JsReader(Target);

impl TextLineReader for JsReader {
    fn read_line(&self, context: &Context) -> FileFuture<Option<TextLine>> {
        self.0.map("readLine", vec![], context, |v| {
            if v.is_undefined() || matches!(v, HostValue::Null) {
                return Ok(None);
            }
            Ok(Some(TextLine {
                text: v
                    .get("text")
                    .and_then(HostValue::as_string)
                    .map(JsonString::to_string_lossy)
                    .unwrap_or_default(),
                terminated: v.get("terminated").and_then(HostValue::as_bool) == Some(true),
            }))
        })
    }
    fn close(&self, context: &Context) -> FileFuture<()> {
        self.0.raw_unit("close", context)
    }
}

impl BinaryReader for JsReader {
    fn info(&self, context: &Context) -> FileFuture<FileInfo> {
        self.0.map("info", vec![], context, |v| parse_info(&v))
    }
    fn read(&self, offset: f64, length: f64, context: &Context) -> FileFuture<Vec<u8>> {
        self.0.map(
            "read",
            vec![HostValue::from(offset), HostValue::from(length)],
            context,
            as_bytes,
        )
    }
    fn scan_lines(
        &self,
        start_line: f64,
        end_line: Option<f64>,
        context: &Context,
    ) -> FileFuture<LineScan> {
        let mut fields = vec![("startLine", HostValue::from(start_line))];
        if let Some(end) = end_line {
            fields.push(("endLine", HostValue::from(end)));
        }
        self.0
            .map("scanLines", vec![HostValue::object(fields)], context, |v| {
                Ok(parse_scan(&v))
            })
    }
    fn close(&self, context: &Context) -> FileFuture<()> {
        self.0.raw_unit("close", context)
    }
}

impl DirReader for JsReader {
    fn next(&self, max_entries: f64, context: &Context) -> FileFuture<DirPage> {
        self.0
            .map("next", vec![HostValue::from(max_entries)], context, |v| {
                Ok(DirPage {
                    entries: match v.get("entries") {
                        Some(HostValue::Array(items)) => {
                            items.iter().map(parse_info).collect::<FileResult<_>>()?
                        }
                        _ => Vec::new(),
                    },
                    done: v.get("done").and_then(HostValue::as_bool) == Some(true),
                })
            })
    }
    fn close(&self, context: &Context) -> FileFuture<()> {
        self.0.raw_unit("close", context)
    }
}

struct JsWatcher(Target);

impl FileWatcher for JsWatcher {
    fn mode(&self) -> WatchMode {
        let mode = call_js(
            &self.0.port,
            Some("get"),
            vec![
                HostValue::Handle(self.0.object.clone()),
                HostValue::from("mode"),
            ],
        );
        match mode {
            Ok(HostValue::String(s)) if s.equals_str("polling") => WatchMode::Polling,
            _ => WatchMode::Native,
        }
    }
    fn close(&self, context: &Context) -> FileFuture<()> {
        self.0.raw_unit("close", context)
    }
}
