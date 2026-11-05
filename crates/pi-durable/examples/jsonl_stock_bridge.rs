//! Test-only transport: the stock pi-mono tests own their filesystem (including
//! failure injections); this process owns every JSONL commit/recovery decision.
use base64::{Engine, engine::general_purpose::STANDARD};
use parking_lot::Mutex;
use pi_chord::context::{ContextKey, create_context_key, with_context_value};
use pi_durable::{env::*, storage::StorageFuture, testing::storage_request, *};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering},
    },
};
static CALL_CONTEXT: LazyLock<ContextKey<u64>> =
    LazyLock::new(|| create_context_key("stock-test context"));
fn encode(value: &impl Serialize) -> JsonValue {
    pi_chord::json::to_value(value).unwrap()
}
fn decode<T: DeserializeOwned>(value: &JsonValue) -> Result<T> {
    serde_json::from_str(&value.to_string()).map_err(Error::from)
}
fn field<T: DeserializeOwned>(value: &JsonValue, key: &str) -> Result<T> {
    decode(&value[key])
}
struct Hub {
    output: Mutex<std::io::Stdout>,
    pending: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<JsonValue>>>,
    next: AtomicU64,
}
impl Hub {
    fn emit(&self, value: JsonValue) {
        let mut output = self.output.lock();
        writeln!(output, "{value}").unwrap();
        output.flush().unwrap();
    }
    fn file<T: DeserializeOwned + Send + 'static>(
        &self,
        fs_id: u64,
        method: &str,
        args: JsonValue,
        context: &Context,
    ) -> FileFuture<T> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (send, receive) = tokio::sync::oneshot::channel();
        self.pending.lock().insert(id, send);
        self.emit(encode(&serde_json::json!({"type":"fs.call","id":id,"fs":fs_id,"method":method,"args":args,"context":context.value(&CALL_CONTEXT).map(|v|*v)})));
        Box::pin(async move {
            let response = receive
                .await
                .map_err(|e| FileError::new(FileErrorCode::Unknown, e.to_string(), None))?;
            if response["ok"].as_bool() == Some(true) {
                decode(&response["value"])
                    .map_err(|e| FileError::new(FileErrorCode::Unknown, e.to_string(), None))
            } else {
                let error = &response["error"];
                let code = match error["code"].to_utf8().as_deref() {
                    Some("aborted") => FileErrorCode::Aborted,
                    Some("not_found") => FileErrorCode::NotFound,
                    Some("permission_denied") => FileErrorCode::PermissionDenied,
                    Some("not_directory") => FileErrorCode::NotDirectory,
                    Some("is_directory") => FileErrorCode::IsDirectory,
                    Some("invalid") => FileErrorCode::Invalid,
                    Some("not_supported") => FileErrorCode::NotSupported,
                    _ => FileErrorCode::Unknown,
                };
                Err(FileError::new(
                    code,
                    error["message"]
                        .to_utf8()
                        .unwrap_or_else(|| "Filesystem bridge failure".into()),
                    error["path"].to_utf8().map(PathBuf::from),
                ))
            }
        })
    }
}
struct RemoteFs {
    hub: Arc<Hub>,
    handle: u64,
    cwd: PathBuf,
    id: String,
}
impl RemoteFs {
    fn call<T: DeserializeOwned + Send + 'static>(
        &self,
        method: &str,
        args: serde_json::Value,
        context: &Context,
    ) -> FileFuture<T> {
        self.hub.file(self.handle, method, encode(&args), context)
    }
}
fn path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
impl FileSystem for RemoteFs {
    fn id(&self) -> &str {
        &self.id
    }
    fn cwd(&self) -> PathBuf {
        self.cwd.clone()
    }
    fn absolute_path(&self, p: &Path, c: &Context) -> FileFuture<PathBuf> {
        self.call("absolutePath", serde_json::json!([path(p)]), c)
    }
    fn join_path(&self, parts: &[PathBuf], c: &Context) -> FileFuture<PathBuf> {
        self.call(
            "joinPath",
            serde_json::json!([parts.iter().map(|p| path(p)).collect::<Vec<_>>()]),
            c,
        )
    }
    fn read_binary_file(&self, p: &Path, c: &Context) -> FileFuture<Vec<u8>> {
        let future = self.call::<String>("readBinaryFile", serde_json::json!([path(p)]), c);
        Box::pin(async move {
            STANDARD
                .decode(future.await?)
                .map_err(|e| FileError::new(FileErrorCode::Invalid, e.to_string(), None))
        })
    }
    fn write_file(&self, p: &Path, content: &[u8], c: &Context) -> FileFuture<()> {
        self.call(
            "writeFile",
            serde_json::json!([path(p), STANDARD.encode(content)]),
            c,
        )
    }
    fn append_file(&self, p: &Path, content: &[u8], c: &Context) -> FileFuture<()> {
        self.call(
            "appendFile",
            serde_json::json!([path(p), STANDARD.encode(content)]),
            c,
        )
    }
    fn truncate_file(&self, p: &Path, size: u64, c: &Context) -> FileFuture<()> {
        self.call("truncateFile", serde_json::json!([path(p), size]), c)
    }
    fn flush_file(&self, p: &Path, c: &Context) -> FileFuture<()> {
        self.call("flushFile", serde_json::json!([path(p)]), c)
    }
    fn rename_file(&self, source: &Path, destination: &Path, c: &Context) -> FileFuture<()> {
        self.call(
            "renameFile",
            serde_json::json!([path(source), path(destination)]),
            c,
        )
    }
    fn list_dir(&self, p: &Path, c: &Context) -> FileFuture<Vec<FileInfo>> {
        let future = self.call::<Vec<JsonValue>>("listDir", serde_json::json!([path(p)]), c);
        Box::pin(async move {
            future
                .await?
                .into_iter()
                .map(|v| {
                    Ok(FileInfo {
                        name: v["name"].to_utf8().unwrap().into(),
                        path: v["path"].to_utf8().unwrap().into(),
                        kind: match v["kind"].to_utf8().as_deref() {
                            Some("file") => FileKind::File,
                            Some("directory") => FileKind::Directory,
                            Some("symlink") => FileKind::Symlink,
                            _ => {
                                return Err(FileError::new(
                                    FileErrorCode::Invalid,
                                    "Unknown file kind",
                                    None,
                                ));
                            }
                        },
                        size: v["size"].as_u64().unwrap(),
                        mtime_ms: v["mtimeMs"].as_f64().unwrap(),
                    })
                })
                .collect()
        })
    }
    fn create_dir(&self, p: &Path, recursive: bool, c: &Context) -> FileFuture<()> {
        self.call(
            "createDir",
            serde_json::json!([path(p),{"recursive":recursive}]),
            c,
        )
    }
    fn remove(&self, p: &Path, recursive: bool, force: bool, c: &Context) -> FileFuture<()> {
        self.call(
            "remove",
            serde_json::json!([path(p),{"recursive":recursive,"force":force}]),
            c,
        )
    }
}
struct Runtime {
    hub: Arc<Hub>,
    stores: Mutex<HashMap<u64, JsonlStorage>>,
    next: AtomicU64,
}
impl Runtime {
    fn request(self: &Arc<Self>, request: JsonValue) -> StorageFuture<Option<JsonValue>> {
        let begin = || -> Result<StorageFuture<Option<JsonValue>>> {
            let method: String = field(&request, "method")?;
            let call_id: u64 = field(&request, "id")?;
            let context_id = field::<u64>(&request, "context").unwrap_or(0);
            let context = with_context_value(&CALL_CONTEXT, context_id, &Context::default());
            if method == "open" {
                let fs = Arc::new(RemoteFs {
                    hub: self.hub.clone(),
                    handle: field(&request, "fs")?,
                    cwd: PathBuf::from(field::<String>(&request, "cwd")?),
                    id: field(&request, "fsNamespace")?,
                });
                let future = JsonlStorage::open(
                    field::<String>(&request, "directory")?,
                    fs,
                    &context,
                    JsonlStorageOptions {
                        fsync: request["options"]["fsync"].as_bool().unwrap_or(false),
                    },
                );
                let runtime = self.clone();
                return Ok(Box::pin(async move {
                    let storage = future.await?;
                    let handle = runtime.next.fetch_add(1, Ordering::Relaxed);
                    runtime.stores.lock().insert(handle, storage);
                    Ok(Some(encode(&handle)))
                }));
            }
            let handle: u64 = field(&request, "handle")?;
            let storage = self
                .stores
                .lock()
                .get(&handle)
                .cloned()
                .ok_or_else(|| Error::message("Unknown native JSONL handle"))?;
            let args = field::<Vec<JsonValue>>(&request, "args")?;
            if method == "commit" {
                let writes: Vec<StorageWrite> = decode(
                    args.first()
                        .ok_or_else(|| Error::message("Missing writes"))?,
                )?;
                let hub = self.hub.clone();
                let future = storage.commit(
                    &writes,
                    Box::new(move |seq| {
                        hub.emit(encode(
                            &serde_json::json!({"type":"adopt","id":call_id,"seq":seq}),
                        ))
                    }),
                    &context,
                );
                return Ok(Box::pin(async move { Ok(Some(encode(&future.await?))) }));
            }
            Ok(storage_request(&storage, &method, &args, Box::new(|_| {})))
        };
        begin().unwrap_or_else(|error| Box::pin(async { Err(error) }))
    }
}
#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let hub = Arc::new(Hub {
        output: Mutex::new(std::io::stdout()),
        pending: Mutex::new(HashMap::new()),
        next: AtomicU64::new(1),
    });
    let runtime = Arc::new(Runtime {
        hub: hub.clone(),
        stores: Mutex::new(HashMap::new()),
        next: AtomicU64::new(1),
    });
    let (send, mut input) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let request = serde_json::from_str::<JsonValue>(&line).unwrap();
            if send.send(request).is_err() {
                break;
            }
        }
    });
    while let Some(request) = input.recv().await {
        if request["type"].string_equals("fs.result") {
            let id: u64 = field(&request, "id")?;
            if let Some(send) = hub.pending.lock().remove(&id) {
                let _ = send.send(request["result"].clone());
            }
            continue;
        }
        let id: u64 = field(&request, "id")?;
        let future = runtime.request(request);
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let result = future.await;
            let response = match result {
                Ok(None) => encode(&serde_json::json!({"id":id,"missing":true})),
                Ok(Some(value)) => {
                    let mut map = pi_chord::json::Map::new();
                    map.insert("id".into(), encode(&id));
                    map.insert("value".into(), value);
                    JsonValue::Object(map.into())
                }
                Err(error) => encode(
                    &serde_json::json!({"id":id,"error":{"name":error.name(),"message":error.to_string()}}),
                ),
            };
            runtime.hub.emit(response);
        });
    }
    Ok(())
}
