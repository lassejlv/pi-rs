//! Test-only RPC for unmodified pi-mono SQLite suites. Storage, schemas and
//! migrations execute in Rust; driver callbacks retain upstream host controls.
use parking_lot::Mutex;
use pi_chord::sqlite::*;
use pi_durable::{
    storage::{StorageFuture, sqlite::*},
    testing::storage_request,
    *,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
fn encode(value: &(impl Serialize + ?Sized)) -> JsonValue {
    pi_chord::json::to_value(value).unwrap()
}
fn decode<T: DeserializeOwned>(value: &JsonValue) -> Result<T> {
    serde_json::from_str(&value.to_string()).map_err(Error::from)
}
fn field<T: DeserializeOwned>(value: &JsonValue, name: &str) -> Result<T> {
    decode(&value[name])
}
struct Transaction {
    callback: SqliteTransactionCallback,
    result: Arc<Mutex<Option<SqliteResult<SqliteTransactionResult>>>>,
}
struct Hub {
    output: Mutex<std::io::Stdout>,
    pending: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<JsonValue>>>,
    transactions: Mutex<HashMap<u64, Transaction>>,
    next: AtomicU64,
}
impl Hub {
    fn emit(&self, value: JsonValue) {
        let mut output = self.output.lock();
        writeln!(output, "{value}").unwrap();
        output.flush().unwrap();
    }
    fn begin(&self, mut frame: JsonValue) -> (u64, tokio::sync::oneshot::Receiver<JsonValue>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        frame
            .as_object_mut()
            .unwrap()
            .insert("id".into(), encode(&id));
        let (send, receive) = tokio::sync::oneshot::channel();
        self.pending.lock().insert(id, send);
        (id, receive)
    }
    fn call(
        &self,
        handle: u64,
        method: &str,
        sql: &str,
        params: &[SqliteValue],
    ) -> SqliteFuture<JsonValue> {
        let params = JsonValue::Array(
            params
                .iter()
                .map(|param| match param {
                    SqliteValue::Null => JsonValue::Null,
                    SqliteValue::Integer(n) => encode(n),
                    SqliteValue::Real(n) => encode(n),
                    SqliteValue::Text(s) => encode(s),
                    SqliteValue::Blob(b) => encode(&serde_json::json!({"blob":b})),
                })
                .collect::<Vec<_>>()
                .into(),
        );
        let mut frame = encode(
            &serde_json::json!({"type":"database.call","handle":handle,"method":method,"sql":sql}),
        );
        frame
            .as_object_mut()
            .unwrap()
            .insert("params".into(), params);
        let (id, receive) = self.begin(frame.clone());
        frame
            .as_object_mut()
            .unwrap()
            .insert("id".into(), encode(&id));
        self.emit(frame);
        Box::pin(async move {
            let response = receive
                .await
                .map_err(|_| SqliteError::new("SQLite host ended"))?;
            if response["error"].is_object() {
                return Err(SqliteError::new(
                    response["error"]["message"].to_utf8().unwrap(),
                ));
            }
            Ok(response["value"].clone())
        })
    }
}
fn row(value: &JsonValue) -> SqliteResult<SqliteRow> {
    let mut result = SqliteRow::new();
    for (key, value) in value
        .as_object()
        .ok_or_else(|| SqliteError::new("Invalid host SQLite row"))?
    {
        let value = if value.is_null() {
            SqliteValue::Null
        } else if let Some(n) = value.as_f64() {
            if n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER as f64 {
                SqliteValue::Integer(n as i64)
            } else {
                SqliteValue::Real(n)
            }
        } else if let Some(s) = value.to_utf8() {
            SqliteValue::Text(s)
        } else {
            return Err(SqliteError::new("Unsupported host SQLite value"));
        };
        result.insert(String::from_utf16(key.units()).unwrap(), value);
    }
    Ok(result)
}
#[derive(Clone)]
struct RemoteDatabase {
    hub: Arc<Hub>,
    handle: u64,
}
impl SqliteExecutor for RemoteDatabase {
    fn exec(&self, sql: &str) -> SqliteFuture<()> {
        let call = self.hub.call(self.handle, "exec", sql, &[]);
        Box::pin(async move {
            call.await?;
            Ok(())
        })
    }
    fn run(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<()> {
        let call = self.hub.call(self.handle, "run", sql, params);
        Box::pin(async move {
            call.await?;
            Ok(())
        })
    }
    fn get(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Option<SqliteRow>> {
        let call = self.hub.call(self.handle, "get", sql, params);
        Box::pin(async move {
            let value = call.await?;
            if value.is_null() {
                Ok(None)
            } else {
                row(&value).map(Some)
            }
        })
    }
    fn all(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Vec<SqliteRow>> {
        let call = self.hub.call(self.handle, "all", sql, params);
        Box::pin(async move {
            call.await?
                .as_array()
                .ok_or_else(|| SqliteError::new("Invalid host SQLite rows"))?
                .iter()
                .map(row)
                .collect()
        })
    }
}
impl SqliteDatabase for RemoteDatabase {
    fn close(&self) -> SqliteFuture<()> {
        let call = self.hub.call(self.handle, "close", "", &[]);
        Box::pin(async move {
            call.await?;
            Ok(())
        })
    }
    fn transaction_raw(
        &self,
        callback: SqliteTransactionCallback,
    ) -> SqliteFuture<SqliteTransactionResult> {
        let result = Arc::new(Mutex::new(None));
        let mut frame =
            encode(&serde_json::json!({"type":"database.transaction","handle":self.handle}));
        let (id, receive) = self.hub.begin(frame.clone());
        self.hub.transactions.lock().insert(
            id,
            Transaction {
                callback,
                result: result.clone(),
            },
        );
        frame
            .as_object_mut()
            .unwrap()
            .insert("id".into(), encode(&id));
        self.hub.emit(frame);
        Box::pin(async move {
            let response = receive
                .await
                .map_err(|_| SqliteError::new("SQLite transaction host ended"))?;
            let result = result.lock().take();
            if let Some(Err(error)) = result {
                return Err(error);
            }
            if response["error"].is_object() {
                return Err(SqliteError::new(
                    response["error"]["message"].to_utf8().unwrap(),
                ));
            }
            result.ok_or_else(|| SqliteError::new("SQLite host skipped transaction callback"))?
        })
    }
}
struct Runtime {
    hub: Arc<Hub>,
    stores: Mutex<HashMap<u64, SqliteStorage>>,
    next: AtomicU64,
}
impl Runtime {
    fn request(self: &Arc<Self>, request: JsonValue) -> StorageFuture<Option<JsonValue>> {
        let begin = || -> Result<StorageFuture<Option<JsonValue>>> {
            let method: String = field(&request, "method")?;
            if method == "configuration" {
                let schema = create_sqlite_schema(None)?;
                let migrations: Vec<_> = create_sqlite_migrations(&schema)
                    .iter()
                    .map(|m| serde_json::json!({"version":m.version,"statements":m.statements}))
                    .collect();
                return Ok(Box::pin(async move {
                    Ok(Some(encode(
                        &serde_json::json!({"schema":schema,"migrations":migrations,"version":CURRENT_SQLITE_SCHEMA_VERSION}),
                    )))
                }));
            }
            if method == "openNative" || method == "open" {
                let prefix = request["options"]["prefix"].to_utf8();
                let opening = if method == "openNative" {
                    open_native_sqlite_storage(
                        field::<String>(&request, "path")?,
                        NativeSqliteStorageOptions {
                            prefix,
                            database: NativeSqliteDatabaseOptions {
                                wal_auto_checkpoint_pages:
                                    request["options"]["walAutoCheckpointPages"]
                                        .as_u64()
                                        .unwrap_or(1000) as u32,
                                busy_timeout_ms: request["options"]["busyTimeoutMs"]
                                    .as_u64()
                                    .unwrap_or(5000),
                            },
                        },
                    )
                } else {
                    SqliteStorage::open(
                        Arc::new(RemoteDatabase {
                            hub: self.hub.clone(),
                            handle: field(&request, "database")?,
                        }),
                        SqliteStorageOptions { prefix },
                    )
                };
                let runtime = self.clone();
                return Ok(Box::pin(async move {
                    let storage = opening.await?;
                    let handle = runtime.next.fetch_add(1, Ordering::Relaxed);
                    runtime.stores.lock().insert(handle, storage);
                    Ok(Some(encode(&handle)))
                }));
            }
            if method == "migrate" {
                let database = RemoteDatabase {
                    hub: self.hub.clone(),
                    handle: field(&request, "database")?,
                };
                let migrations = if let Some(values) = request["migrations"].as_array() {
                    Some(
                        values
                            .iter()
                            .map(|m| {
                                Ok(SqliteMigration {
                                    version: field(m, "version")?,
                                    statements: field(m, "statements")?,
                                })
                            })
                            .collect::<Result<Vec<_>>>()?,
                    )
                } else {
                    None
                };
                let future = pi_durable::storage::sqlite::apply_sqlite_migrations(
                    &database,
                    migrations.as_deref(),
                );
                return Ok(Box::pin(async move {
                    future.await?;
                    Ok(None)
                }));
            }
            let handle = field(&request, "handle")?;
            let storage = self
                .stores
                .lock()
                .get(&handle)
                .cloned()
                .ok_or_else(|| Error::message("Unknown SQLite storage handle"))?;
            let args: Vec<JsonValue> = field(&request, "args")?;
            let call_id: u64 = field(&request, "id")?;
            let hub = self.hub.clone();
            Ok(storage_request(
                &storage,
                &method,
                &args,
                Box::new(move |seq| {
                    hub.emit(encode(
                        &serde_json::json!({"type":"adopt","id":call_id,"seq":seq}),
                    ))
                }),
            ))
        };
        begin().unwrap_or_else(|error| Box::pin(async { Err(error) }))
    }
}
#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let hub = Arc::new(Hub {
        output: Mutex::new(std::io::stdout()),
        pending: Mutex::new(HashMap::new()),
        transactions: Mutex::new(HashMap::new()),
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
            let frame: JsonValue = serde_json::from_str(&line).unwrap();
            if send.send(frame).is_err() {
                break;
            }
        }
    });
    while let Some(frame) = input.recv().await {
        let id: u64 = field(&frame, "id")?;
        if frame["type"].string_equals("database.result") {
            if let Some(send) = hub.pending.lock().remove(&id) {
                let _ = send.send(frame);
            }
        } else if frame["type"].string_equals("transaction.begin") {
            let transaction = hub
                .transactions
                .lock()
                .remove(&id)
                .ok_or("Unknown host transaction")?;
            let executor = Arc::new(RemoteDatabase {
                hub: hub.clone(),
                handle: field(&frame, "handle")?,
            });
            let future = (transaction.callback)(executor);
            let hub = hub.clone();
            tokio::spawn(async move {
                let result = future.await;
                let error = result
                    .as_ref()
                    .err()
                    .map(|e| serde_json::json!({"message":e.to_string()}));
                *transaction.result.lock() = Some(result);
                hub.emit(encode(
                    &serde_json::json!({"type":"transaction.result","id":id,"error":error}),
                ));
            });
        } else {
            // Begin every request in incoming order, before spawning its waiter.
            let admit = frame["admit"].as_bool().unwrap_or(false);
            let future = runtime.request(frame);
            let hub = hub.clone();
            if admit {
                hub.emit(encode(&serde_json::json!({"type":"admitted","id":id})));
            }
            tokio::spawn(async move {
                let mut response = pi_chord::json::Map::new();
                response.insert("id".into(), encode(&id));
                match future.await {
                    Ok(Some(value)) => {
                        response.insert("value".into(), value);
                    }
                    Ok(None) => {
                        response.insert("missing".into(), encode(&true));
                    }
                    Err(error) => {
                        response.insert("error".into(), encode(&serde_json::json!({"name":error.name(),"message":error.to_string()})));
                    }
                }
                hub.emit(JsonValue::Object(response.into()));
            });
        }
    }
    if !hub.pending.lock().is_empty() || !hub.transactions.lock().is_empty() {
        return Err("SQLite host ended with pending calls".into());
    }
    Ok(())
}
