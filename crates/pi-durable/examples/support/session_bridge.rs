//! Test-only access to the Session kernel without Harness bootstrap or scheduling.
use super::*;

impl Hub {
    pub(super) fn session_request(
        self: &Arc<Self>,
        method: &str,
        request: JsonValue,
        _context: Context,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        match method {
            "session.open" => {
                let id = field(&request, "id")?;
                let handle = field(&request, "storage")?;
                let storage: Arc<dyn Storage> =
                    if let Some(memory) = request["nativeMemory"].as_u64() {
                        self.memory.direct_opens.fetch_add(1, Ordering::Relaxed);
                        self.memory_storage(memory)?
                    } else {
                        Arc::new(remote_storage::RemoteStorage {
                            hub: self.clone(),
                            handle,
                        })
                    };
                let clock = Arc::downgrade(self);
                let retention = Arc::downgrade(self);
                let created = Arc::downgrade(self);
                let closing = Arc::downgrade(self);
                let both = Arc::downgrade(self);
                // A host that answers `session.clock` reads the clock and the retention of one
                // bookkeeping step with one callback (`{ now, retention }`, null for a failed read).
                let clock_reader = (request["clock"].as_bool() == Some(true)).then(|| {
                    Arc::new(move || {
                        let Some(value) = both
                            .upgrade()
                            .and_then(|hub| hub.sync("session.clock", encode(&id)).ok())
                        else {
                            return (None, None);
                        };
                        let number = |value: &JsonValue| match value.to_utf8().as_deref() {
                            Some("Infinity") => Some(f64::INFINITY),
                            Some("NaN") => Some(f64::NAN),
                            _ => value.as_f64(),
                        };
                        (number(&value["now"]), number(&value["retention"]))
                    }) as pi_durable::session::ClockReader
                });
                let options = SessionOptions {
                    clock_reader,
                    now: Arc::new(move || {
                        clock
                            .upgrade()
                            .expect("Session test transport closed")
                            .sync("session.now", encode(&id))
                            .and_then(|v| decode(&v))
                            .unwrap_or_else(|error| std::panic::panic_any(error))
                    }),
                    document_retention_reader: Some(Arc::new(move || {
                        let value = retention
                            .upgrade()
                            .ok_or_else(|| Error::message("Session test transport closed"))?
                            .sync("session.retention", encode(&id))?;
                        Ok(Some(match value.to_utf8().as_deref() {
                            Some("Infinity") => f64::INFINITY,
                            Some("NaN") => f64::NAN,
                            _ => value.as_f64().unwrap_or(600_000.0),
                        }))
                    })),
                    read_only: request["readOnly"].as_bool().unwrap_or(false),
                    set_timer: request["hostTimers"]
                        .as_bool()
                        .unwrap_or(false)
                        .then(|| self.host_set_timer(id)),
                    conversation_created: Some(Arc::new(move |tx, record| {
                        let hub = created.upgrade();
                        Box::pin(async move {
                            let hub =
                                hub.ok_or_else(|| Error::message("Session test transport closed"))?;
                            let tx_id = hub.next.fetch_add(1, Ordering::Relaxed);
                            hub.transactions.lock().insert(tx_id, tx);
                            let result = hub
                                .call(
                                    "session.conversationCreated",
                                    object([
                                        ("harness", encode(&id)),
                                        ("tx", encode(&tx_id)),
                                        ("record", encode(&record)),
                                    ]),
                                )
                                .await;
                            hub.transactions.lock().remove(&tx_id);
                            result.map(|_| ())
                        })
                    })),
                    before_close: Some(Arc::new(move |_| {
                        let hub = closing.upgrade();
                        Box::pin(async move {
                            hub.ok_or_else(|| Error::message("Session test transport closed"))?
                                .call("session.beforeClose", encode(&id))
                                .await
                                .map(|_| ())
                        })
                    })),
                    ..Default::default()
                };
                let session = Session::new(storage, options);
                if request["transactionProbes"].as_bool() == Some(true) {
                    #[cfg(feature = "stock-test-probes")]
                    {
                        let host = Arc::downgrade(self);
                        session.set_transaction_probe(move |point| {
                            let host = host
                                .upgrade()
                                .ok_or_else(|| Error::message("Session test transport closed"))?;
                            let (method, seq) = match point {
                                TransactionProbePoint::Adopt(seq) => ("adopt", Some(seq)),
                                TransactionProbePoint::Discard => ("discard", None),
                            };
                            host.sync(
                                "session.transactionProbe",
                                object([("method", encode(&method)), ("seq", encode(&seq))]),
                            )
                            .map(|_| ())
                        });
                    }
                    #[cfg(not(feature = "stock-test-probes"))]
                    return Err(Error::message(
                        "Transaction probes require stock-test-probes",
                    ));
                }
                self.sessions.lock().insert(id, session);
                Ok(Box::pin(async move { Ok(Some(encode(&id))) }))
            }
            "session.cache" => {
                let sizes = self.session(&request)?.cache_sizes();
                let value = object([
                    ("cached", encode(&sizes.cached)),
                    ("idle", encode(&sizes.idle)),
                    ("attached", encode(&sizes.attached)),
                    ("sweeps", encode(&sizes.sweep_jobs)),
                ]);
                Ok(Box::pin(async move { Ok(Some(value)) }))
            }
            "session.readLine" => {
                let session = self.session(&request)?;
                let callback: u64 = field(&request, "callback")?;
                let hub = self.clone();
                Ok(session.read_on_line(move || async move {
                    hub.call("session.readLine", encode(&callback)).await
                }))
            }
            other => Err(Error::message(format!("Unknown Session method {other}"))),
        }
    }
}
