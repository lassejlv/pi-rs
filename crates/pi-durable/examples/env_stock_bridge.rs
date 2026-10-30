//! Test-only process transport. Filesystem and reader behavior lives in env.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use parking_lot::Mutex;
use pi_chord::{
    AbortController,
    context::{create_context_key, with_abort_signal, with_context_value},
};
use pi_durable::{Context, env::*};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    path::PathBuf,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering},
    },
};
static REQUEST: LazyLock<pi_chord::context::ContextKey<u64>> =
    LazyLock::new(|| create_context_key("stock environment request"));
type Reply = tokio::sync::oneshot::Sender<Value>;
#[derive(Clone)]
enum Reader {
    Text(Arc<dyn TextLineReader>),
    Binary(Arc<dyn BinaryReader>),
    Directory(Arc<dyn DirReader>),
}
struct Hub {
    next: AtomicU64,
    output: Mutex<std::io::Stdout>,
    pending: Mutex<HashMap<u64, Reply>>,
    contexts: Mutex<HashMap<u64, AbortController>>,
    filesystems: Mutex<HashMap<u64, Arc<NativeFileSystem>>>,
    readers: Mutex<HashMap<u64, Reader>>,
    calls: AtomicU64,
    opens: AtomicU64,
    text_reads: AtomicU64,
    binary_reads: AtomicU64,
    shell_calls: AtomicU64,
    shell_outputs: AtomicU64,
    watches: Mutex<HashMap<u64, Arc<Watched>>>,
    watch_opens: AtomicU64,
    native_watch_opens: AtomicU64,
    polling_watch_opens: AtomicU64,
    watch_frames: AtomicU64,
    watch_closes: AtomicU64,
}
struct Watched {
    watcher: Arc<dyn FileWatcher>,
    closed: std::sync::atomic::AtomicBool,
}
fn code(value: FileErrorCode) -> &'static str {
    value.as_str()
}
fn file_error(value: &Value) -> FileError {
    FileError::new(
        FileErrorCode::from_name(value["code"].as_str().unwrap_or_default()),
        value["message"]
            .as_str()
            .unwrap_or("Host file operation failed"),
        value["path"].as_str().map(PathBuf::from),
    )
}
fn info(value: FileInfo) -> Value {
    json!({"name":value.name.to_string_lossy(),"path":value.path.to_string_lossy(),"kind":match value.kind{FileKind::File=>"file",FileKind::Directory=>"directory",FileKind::Symlink=>"symlink"},"size":value.size,"mtimeMs":value.mtime_ms})
}
fn parse_info(value: Value) -> FileResult<FileInfo> {
    Ok(FileInfo {
        name: value["name"].as_str().unwrap().into(),
        path: value["path"].as_str().unwrap().into(),
        kind: match value["kind"].as_str() {
            Some("file") => FileKind::File,
            Some("directory") => FileKind::Directory,
            Some("symlink") => FileKind::Symlink,
            _ => {
                return Err(FileError::new(
                    FileErrorCode::Invalid,
                    "Unsupported file type",
                    value["path"].as_str().map(PathBuf::from),
                ));
            }
        },
        size: value["size"].as_u64().unwrap(),
        mtime_ms: value["mtimeMs"].as_f64().unwrap(),
    })
}
fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .unwrap_or(f64::NAN)
}
fn failed(message: &str) -> FileError {
    FileError::new(FileErrorCode::Invalid, message, None)
}
impl Hub {
    fn emit(&self, value: Value) {
        let mut output = self.output.lock();
        writeln!(output, "{value}").unwrap();
        output.flush().unwrap();
    }
    fn call(&self, method: &str, args: Value, context: &Context) -> FileFuture<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (send, receive) = tokio::sync::oneshot::channel();
        self.pending.lock().insert(id, send);
        self.emit(json!({"type":"callback","id":id,"method":method,"args":args,"context":context.value(&REQUEST).map(|id|*id)}));
        Box::pin(async move {
            let value = receive.await.map_err(|error| failed(&error.to_string()))?;
            if value["ok"].as_bool() == Some(true) {
                Ok(value["value"].clone())
            } else {
                Err(file_error(&value["error"]))
            }
        })
    }
    async fn handle(self: Arc<Self>, request: Value, context: Context) -> FileResult<Value> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let method = request["method"].as_str().unwrap();
        if method == "stats" {
            return Ok(
                json!({"opens":self.opens.load(Ordering::Relaxed),"calls":self.calls.load(Ordering::Relaxed),"textReads":self.text_reads.load(Ordering::Relaxed),"binaryReads":self.binary_reads.load(Ordering::Relaxed),"shellCalls":self.shell_calls.load(Ordering::Relaxed),"shellOutputs":self.shell_outputs.load(Ordering::Relaxed),"watchOpens":self.watch_opens.load(Ordering::Relaxed),"nativeWatchOpens":self.native_watch_opens.load(Ordering::Relaxed),"pollingWatchOpens":self.polling_watch_opens.load(Ordering::Relaxed),"watchFrames":self.watch_frames.load(Ordering::Relaxed),"watchCloses":self.watch_closes.load(Ordering::Relaxed),"activeWatches":self.watches.lock().values().filter(|watch|!watch.closed.load(Ordering::Acquire)).count()}),
            );
        }
        if method == "fs.new" {
            let opener = if request["hostIo"].as_bool() == Some(true) {
                let hub = Arc::downgrade(&self);
                Some(Arc::new(
                    move |path: PathBuf,
                          options: OpenFileOptions,
                          context: Context|
                          -> FileFuture<Arc<dyn FileHandle>> {
                        let Some(owner) = hub.upgrade() else {
                            return Box::pin(async { Err(failed("Environment host is closed")) });
                        };
                        let operation=owner.call("file.open",json!({"path":path,"write":options.write,"regular":options.regular,"noFollow":options.no_follow}),&context);
                        let hub = hub.clone();
                        Box::pin(async move {
                            let id = operation.await?.as_u64().unwrap();
                            Ok(Arc::new(HostFile { hub, id }) as _)
                        })
                    },
                ) as FileOpener)
            } else {
                None
            };
            let cwd = request["cwd"].as_str().unwrap();
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            let weak = Arc::downgrade(&self);
            let options = NativeShellOptions {
                shell_path: request["shellPath"].as_str().map(PathBuf::from),
                environment: request["shellEnv"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(key, value)| (key.into(), value.as_str().map(Into::into)))
                    .collect(),
                temp_file: Some(Arc::new(move |context| {
                    let operation = weak.upgrade().unwrap().call(
                        "shell.temp",
                        json!({"environment":id}),
                        &context,
                    );
                    Box::pin(async move { Ok(PathBuf::from(operation.await?.as_str().unwrap())) })
                })),
                spill_opener: if request["hostSpill"].as_bool() == Some(true) {
                    let hub = Arc::downgrade(&self);
                    Some(Arc::new(move |path| {
                        let owner = hub.upgrade().unwrap();
                        let operation =
                            owner.call("spill.open", json!({"path":path}), &Context::default());
                        let hub = hub.clone();
                        Box::pin(async move {
                            let id = operation
                                .await
                                .map_err(host_execution_error)?
                                .as_u64()
                                .unwrap();
                            Ok(Arc::new(HostSpill { hub, id }) as Arc<dyn ShellSpillWriter>)
                        })
                    }))
                } else {
                    None
                },
                ..Default::default()
            };
            let watch = &request["watch"];
            let watch_options = NativeWatchOptions {
                mode: watch
                    .get("mode")
                    .filter(|value| !value.is_null())
                    .map(|value| {
                        serde_json::from_value(value.clone())
                            .map_err(|error| failed(&error.to_string()))
                    })
                    .transpose()?,
                poll_interval_ms: watch
                    .get("pollIntervalMs")
                    .filter(|value| !value.is_null())
                    .map(number),
                max_directories: watch
                    .get("maxDirectories")
                    .filter(|value| !value.is_null())
                    .map(number),
                ..Default::default()
            };
            let filesystem = Arc::new(
                (match opener {
                    Some(opener) => NativeFileSystem::with_file_opener(cwd, opener)?,
                    None => NativeFileSystem::new(cwd)?,
                })
                .with_shell_options(options)
                .with_watch_options(watch_options),
            );
            let value = json!({"id":id,"namespace":filesystem.id()});
            self.filesystems.lock().insert(id, filesystem);
            self.opens.fetch_add(1, Ordering::Relaxed);
            return Ok(value);
        }
        if method.starts_with("watch.") {
            let watch = self
                .watches
                .lock()
                .get(&request["watcher"].as_u64().unwrap())
                .cloned()
                .ok_or_else(|| failed("Unknown native watcher"))?;
            return match method {
                "watch.mode" => Ok(json!(watch.watcher.mode())),
                "watch.close" => {
                    watch.watcher.close(&context).await?;
                    if !watch.closed.swap(true, Ordering::AcqRel) {
                        self.watch_closes.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Value::Null)
                }
                _ => Err(failed("Unknown native watcher operation")),
            };
        }
        if method.starts_with("reader.") {
            let id = request["reader"].as_u64().unwrap();
            let reader = self
                .readers
                .lock()
                .get(&id)
                .cloned()
                .ok_or_else(|| failed("Unknown native reader"))?;
            return match (method, reader) {
                ("reader.line", Reader::Text(reader)) => {
                    self.text_reads.fetch_add(1, Ordering::Relaxed);
                    Ok(reader
                        .read_line(&context)
                        .await?
                        .map(|line| json!({"text":line.text,"terminated":line.terminated}))
                        .unwrap_or(Value::Null))
                }
                ("reader.info", Reader::Binary(reader)) => Ok(info(reader.info(&context).await?)),
                ("reader.read", Reader::Binary(reader)) => {
                    self.binary_reads.fetch_add(1, Ordering::Relaxed);
                    Ok(json!(
                        STANDARD.encode(
                            reader
                                .read(
                                    number(&request["offset"]),
                                    number(&request["length"]),
                                    &context
                                )
                                .await?
                        )
                    ))
                }
                ("reader.scan", Reader::Binary(reader)) => Ok(serde_json::to_value(
                    reader
                        .scan_lines(
                            number(&request["start"]),
                            request.get("end").map(number),
                            &context,
                        )
                        .await?,
                )
                .unwrap()),
                ("reader.next", Reader::Directory(reader)) => {
                    let page = reader.next(number(&request["max"]), &context).await?;
                    Ok(
                        json!({"entries":page.entries.into_iter().map(info).collect::<Vec<_>>(),"done":page.done}),
                    )
                }
                ("reader.close", Reader::Text(reader)) => {
                    reader.close(&context).await?;
                    Ok(Value::Null)
                }
                ("reader.close", Reader::Binary(reader)) => {
                    reader.close(&context).await?;
                    Ok(Value::Null)
                }
                ("reader.close", Reader::Directory(reader)) => {
                    reader.close(&context).await?;
                    Ok(Value::Null)
                }
                _ => Err(failed("Unknown reader operation")),
            };
        }
        let filesystem = self
            .filesystems
            .lock()
            .get(&request["fs"].as_u64().unwrap())
            .cloned()
            .ok_or_else(|| failed("Unknown native filesystem"))?;
        let path = PathBuf::from(request["path"].as_str().unwrap_or(""));
        match method {
            "fs.watch" => {
                let targets = request["targets"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|target| WatchTarget {
                        path: target["path"].as_str().unwrap().into(),
                        recursive: target["recursive"].as_bool().unwrap_or(false),
                        exclude: WatchExclude {
                            hidden: target["exclude"]["hidden"].as_bool().unwrap_or(false),
                            names: target["exclude"]["names"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|name| name.as_str().unwrap().into())
                                .collect(),
                        },
                    })
                    .collect::<Vec<_>>();
                let callback = request["callback"].as_u64().unwrap();
                let weak = Arc::downgrade(&self);
                let watcher=filesystem.watch(&targets,Arc::new(move|change|{
                    let Some(hub)=weak.upgrade()else{return;};
                    hub.watch_frames.fetch_add(1,Ordering::Relaxed);
                    let change=match change{
                        WatchChange::Paths(paths)=>json!({"paths":paths}),
                        WatchChange::Overflow=>json!({"overflow":true}),
                        WatchChange::Error(error)=>json!({"error":{"kind":"file","code":code(error.code()),"message":error.to_string(),"path":error.path()}})
                    };
                    let operation=hub.call("watch.change",json!({"callback":callback,"change":change}),&Context::default());
                    let _=tokio::task::block_in_place(||tokio::runtime::Handle::current().block_on(operation));
                }),&context).await?;
                let mode = watcher.mode();
                match mode {
                    WatchMode::Native => &self.native_watch_opens,
                    WatchMode::Polling => &self.polling_watch_opens,
                }
                .fetch_add(1, Ordering::Relaxed);
                let id = self.next.fetch_add(1, Ordering::Relaxed);
                self.watches.lock().insert(
                    id,
                    Arc::new(Watched {
                        watcher,
                        closed: std::sync::atomic::AtomicBool::new(false),
                    }),
                );
                self.watch_opens.fetch_add(1, Ordering::Relaxed);
                Ok(json!({"id":id,"mode":mode}))
            }
            "fs.cwd" => {
                filesystem.set_cwd(&path);
                Ok(Value::Null)
            }
            "fs.absolute" => Ok(json!(filesystem.absolute_path(&path, &context).await?)),
            "fs.join" => {
                let parts = request["parts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|value| PathBuf::from(value.as_str().unwrap()))
                    .collect::<Vec<_>>();
                Ok(json!(filesystem.join_path(&parts, &context).await?))
            }
            "fs.readText" => Ok(json!(filesystem.read_text_file(&path, &context).await?)),
            "fs.readLines" => Ok(json!(
                filesystem
                    .read_text_lines(&path, request.get("max").map(number), &context)
                    .await?
            )),
            "fs.readBinary" => Ok(json!(
                STANDARD.encode(filesystem.read_binary_file(&path, &context).await?)
            )),
            "fs.write" | "fs.append" => {
                let bytes = STANDARD.decode(request["data"].as_str().unwrap()).unwrap();
                if method == "fs.write" {
                    filesystem.write_file(&path, &bytes, &context).await?;
                } else {
                    filesystem.append_file(&path, &bytes, &context).await?;
                }
                Ok(Value::Null)
            }
            "fs.truncate" => {
                let size = number(&request["size"]);
                if !size.is_finite()
                    || size < 0.0
                    || size.fract() != 0.0
                    || size > pi_durable::MAX_SAFE_INTEGER as f64
                {
                    return Err(FileError::new(
                        FileErrorCode::Invalid,
                        "File size must be a non-negative safe integer",
                        Some(filesystem.absolute_path(&path, &context).await?),
                    ));
                }
                filesystem
                    .truncate_file(&path, size as u64, &context)
                    .await?;
                Ok(Value::Null)
            }
            "fs.flush" => {
                filesystem.flush_file(&path, &context).await?;
                Ok(Value::Null)
            }
            "fs.rename" => {
                filesystem
                    .rename_file(
                        &path,
                        &PathBuf::from(request["destination"].as_str().unwrap()),
                        &context,
                    )
                    .await?;
                Ok(Value::Null)
            }
            "fs.info" => Ok(info(filesystem.file_info(&path, &context).await?)),
            "fs.list" => Ok(json!(
                filesystem
                    .list_dir(&path, &context)
                    .await?
                    .into_iter()
                    .map(info)
                    .collect::<Vec<_>>()
            )),
            "fs.canonical" => Ok(json!(filesystem.canonical_path(&path, &context).await?)),
            "fs.exists" => Ok(json!(filesystem.exists(&path, &context).await?)),
            "fs.mkdir" => {
                filesystem
                    .create_dir(
                        &path,
                        request["recursive"].as_bool().unwrap_or(true),
                        &context,
                    )
                    .await?;
                Ok(Value::Null)
            }
            "fs.remove" => {
                filesystem
                    .remove(
                        &path,
                        request["recursive"].as_bool().unwrap_or(false),
                        request["force"].as_bool().unwrap_or(false),
                        &context,
                    )
                    .await?;
                Ok(Value::Null)
            }
            "fs.tempDir" => Ok(json!(
                filesystem
                    .create_temp_dir(request["prefix"].as_str(), &context)
                    .await?
            )),
            "fs.tempFile" => Ok(json!(
                filesystem
                    .create_temp_file(
                        request["prefix"].as_str(),
                        request["suffix"].as_str(),
                        &context
                    )
                    .await?
            )),
            "fs.cleanup" => {
                filesystem
                    .cleanup(&context)
                    .await
                    .map_err(|error| failed(&error.to_string()))?;
                Ok(Value::Null)
            }
            "fs.openText" | "fs.openBinary" | "fs.openDir" => {
                let reader = match method {
                    "fs.openText" => {
                        Reader::Text(filesystem.open_text_line_reader(&path, &context).await?)
                    }
                    "fs.openBinary" => Reader::Binary(
                        filesystem
                            .open_binary_reader(
                                &path,
                                request["noFollow"].as_bool().unwrap_or(false),
                                &context,
                            )
                            .await?,
                    ),
                    _ => Reader::Directory(filesystem.open_dir_reader(&path, &context).await?),
                };
                let id = self.next.fetch_add(1, Ordering::Relaxed);
                self.readers.lock().insert(id, reader);
                Ok(json!(id))
            }
            _ => Err(failed("Unknown native filesystem operation")),
        }
    }
    async fn exec(self: Arc<Self>, request: Value, context: Context) -> ExecutionResult<Value> {
        self.shell_calls.fetch_add(1, Ordering::Relaxed);
        let filesystem = self
            .filesystems
            .lock()
            .get(&request["fs"].as_u64().unwrap())
            .cloned()
            .ok_or_else(|| {
                ExecutionError::new(ExecutionErrorCode::Unknown, "Unknown environment")
            })?;
        let host = &request["host"];
        let filesystem = filesystem.as_ref().clone().with_shell_host(ShellHost {
            cwd: host["cwd"].as_str().unwrap().into(),
            platform: if host["platform"].as_str() == Some("win32") {
                ShellPlatform::Windows
            } else {
                ShellPlatform::Unix
            },
            environment: host["env"]
                .as_object()
                .unwrap()
                .iter()
                .filter_map(|(key, value)| value.as_str().map(|value| (key.into(), value.into())))
                .collect(),
        });
        let command: ShellCommand =
            serde_json::from_value(request["command"].clone()).map_err(|error| {
                ExecutionError::new(ExecutionErrorCode::SpawnError, error.to_string())
            })?;
        let input = &request["options"];
        let mut options = ShellExecOptions {
            cwd: input["cwd"].as_str().map(PathBuf::from),
            env: input
                .get("env")
                .map(|env| serde_json::from_value(env.clone()).unwrap()),
            inherit_env: input["inheritEnv"].as_bool(),
            timeout: input.get("timeout").map(number),
            spill: input.get("spill").map(|spill| ShellSpillOptions {
                after_bytes: number(&spill["afterBytes"]),
                after_lines: number(&spill["afterLines"]),
            }),
            ..Default::default()
        };
        if let Some(callback) = input["onOutput"].as_u64() {
            let weak = Arc::downgrade(&self);
            options.on_output = Some(Arc::new(move |text, context, info| {
                let hub = weak.upgrade().unwrap();
                hub.shell_outputs.fetch_add(1, Ordering::Relaxed);
                let operation=hub.call("shell.output",json!({"callback":callback,"text":text.to_string_lossy(),"stream":match info.stream{ShellStream::Stdout=>"stdout",ShellStream::Stderr=>"stderr"}}),&context);
                // A synchronous host callback must leave Tokio's cooperative
                // poll budget before waiting for its transport response.
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(operation)
                })
                .map_err(|error| {
                    ExecutionError::new(ExecutionErrorCode::CallbackError, error.to_string())
                })?;
                Ok(())
            }));
        }
        let result = filesystem.exec(command, options, &context).await?;
        Ok(json!({"exitCode":result.exit_code,"spillPath":result.spill_path}))
    }
}
fn execution_error(error: ExecutionError) -> Value {
    json!({"kind":"execution","name":"ExecutionError","code":error.code().as_str(),"message":error.to_string(),"spillPath":error.spill_path()})
}
fn host_execution_error(error: FileError) -> ExecutionError {
    ExecutionError::new(ExecutionErrorCode::Unknown, error.to_string()).with_cause(Arc::new(error))
}
struct HostSpill {
    hub: std::sync::Weak<Hub>,
    id: u64,
}
impl HostSpill {
    fn call(&self, method: &str, args: Value) -> ExecutionFuture<Value> {
        let operation = self
            .hub
            .upgrade()
            .unwrap()
            .call(method, args, &Context::default());
        Box::pin(async move { operation.await.map_err(host_execution_error) })
    }
    fn wait(&self, method: &str) -> ExecutionFuture<()> {
        let operation = self.call(method, json!({"stream":self.id}));
        Box::pin(async move {
            operation.await?;
            Ok(())
        })
    }
}
impl ShellSpillWriter for HostSpill {
    fn write(&self, bytes: Vec<u8>) -> ExecutionFuture<bool> {
        let operation = self.call(
            "spill.write",
            json!({"stream":self.id,"bytes":STANDARD.encode(bytes)}),
        );
        Box::pin(async move { Ok(operation.await?.as_bool().unwrap()) })
    }
    fn drain(&self) -> ExecutionFuture<()> {
        self.wait("spill.drain")
    }
    fn finish(&self) -> ExecutionFuture<()> {
        self.wait("spill.finish")
    }
    fn settled(&self) -> ExecutionFuture<()> {
        self.wait("spill.settled")
    }
}
struct HostFile {
    hub: std::sync::Weak<Hub>,
    id: u64,
}
impl HostFile {
    fn call(&self, method: &str, args: Value, context: &Context) -> FileFuture<Value> {
        match self.hub.upgrade() {
            Some(hub) => hub.call(method, args, context),
            None => Box::pin(async { Err(failed("Environment host is closed")) }),
        }
    }
}
impl FileHandle for HostFile {
    fn info(&self, context: &Context) -> FileFuture<FileInfo> {
        let operation = self.call("file.info", json!({"file":self.id}), context);
        Box::pin(async move { parse_info(operation.await?) })
    }
    fn read(&self, offset: u64, length: usize, context: &Context) -> FileFuture<Vec<u8>> {
        let operation = self.call(
            "file.read",
            json!({"file":self.id,"offset":offset,"length":length}),
            context,
        );
        Box::pin(async move {
            STANDARD
                .decode(operation.await?.as_str().unwrap())
                .map_err(|error| failed(&error.to_string()))
        })
    }
    fn sync(&self, context: &Context) -> FileFuture<()> {
        let operation = self.call("file.sync", json!({"file":self.id}), context);
        Box::pin(async move {
            operation.await?;
            Ok(())
        })
    }
    fn close(&self, context: &Context) -> FileFuture<()> {
        let operation = self.call("file.close", json!({"file":self.id}), context);
        Box::pin(async move {
            operation.await?;
            Ok(())
        })
    }
}
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let hub = Arc::new(Hub {
        next: AtomicU64::new(1_000_000),
        output: Mutex::new(std::io::stdout()),
        pending: Mutex::new(HashMap::new()),
        contexts: Mutex::new(HashMap::new()),
        filesystems: Mutex::new(HashMap::new()),
        readers: Mutex::new(HashMap::new()),
        calls: AtomicU64::new(0),
        opens: AtomicU64::new(0),
        text_reads: AtomicU64::new(0),
        binary_reads: AtomicU64::new(0),
        shell_calls: AtomicU64::new(0),
        shell_outputs: AtomicU64::new(0),
        watches: Mutex::new(HashMap::new()),
        watch_opens: AtomicU64::new(0),
        native_watch_opens: AtomicU64::new(0),
        polling_watch_opens: AtomicU64::new(0),
        watch_frames: AtomicU64::new(0),
        watch_closes: AtomicU64::new(0),
    });
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move||{
        for line in std::io::stdin().lock().lines(){
            let request:Value=serde_json::from_str(&line.unwrap()).unwrap();let id=request["id"].as_u64().unwrap();
            match request["type"].as_str(){
                Some("callback.result")=>{if let Some(send)=hub.pending.lock().remove(&id){let _=send.send(request["value"].clone());}continue;},
                Some("abort")=>{if let Some(controller)=hub.contexts.lock().get(&id){controller.abort();}continue;},
                _=>{},
            }
            let controller=AbortController::new();if request.get("abortInitially").is_some(){controller.abort();}
            let context=with_context_value(&REQUEST,id,&with_abort_signal(controller.signal(),&Context::default()));
            hub.contexts.lock().insert(id,controller);
            let hub=hub.clone();runtime.spawn(async move{
                let result=if request["method"].as_str()==Some("fs.exec") {
                    hub.clone().exec(request,context).await.map_err(execution_error)
                } else {
                    hub.clone().handle(request,context).await.map_err(|error|json!({"kind":"file","name":"FileError","code":code(error.code()),"message":error.to_string(),"path":error.path()}))
                };
                hub.emit(match result {Ok(value)=>json!({"id":id,"value":value}),Err(error)=>json!({"id":id,"error":error})});
                hub.contexts.lock().remove(&id);
            });
        }
        for (_,send) in hub.pending.lock().drain(){let _=send.send(json!({"ok":false,"error":{"message":"Host closed"}}));}
    }).await.unwrap();
}
