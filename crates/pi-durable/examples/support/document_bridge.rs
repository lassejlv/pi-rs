//! Dynamic definitions and operation-bearing drafts over the native document API.
use super::*;
use pi_chord::delta::Op;
#[derive(Default)]
pub(super) struct DocumentBridge {
    definitions: Mutex<HashMap<u64, Arc<AnyDocDefinition>>>,
    drafts: Mutex<HashMap<u64, (u64, DocDraft<JsonObject>)>>,
    pub reads: AtomicU64,
    pub writes: AtomicU64,
    pub watches: AtomicU64,
    pub watch_frames: AtomicU64,
}
impl Hub {
    fn document_definition(
        self: &Arc<Self>,
        description: &JsonValue,
    ) -> Result<Arc<AnyDocDefinition>> {
        let id = field(description, "id")?;
        if let Some(definition) = self.documents.definitions.lock().get(&id).cloned() {
            return Ok(definition);
        }
        let scope = match description["scope"].to_utf8().as_deref() {
            Some("session") => DefinitionScope::Session,
            Some("task") => DefinitionScope::Task,
            Some("conversation") => DefinitionScope::Conversation {
                history: match description["history"].to_utf8().as_deref() {
                    Some("latest") => ConversationHistory::Latest {
                        fork: field(description, "fork")?,
                    },
                    Some("rewindable") => ConversationHistory::Rewindable {
                        fork: field(description, "fork")?,
                    },
                    _ => return Err(Error::message("Invalid document history")),
                },
            },
            _ => return Err(Error::message("Invalid document scope")),
        };
        let initial = self.clone();
        let checkpoint = self.clone();
        let mut definition = AnyDocDefinition::new_with_shared_checkpoint(
            field::<JsonString>(description, "kind")?,
            field(description, "version")?,
            scope,
            description["family"].as_bool().unwrap_or(false),
            move |seed| {
                let mut args = object([("id", encode(&id))]);
                if let Some(seed) = seed {
                    args.as_object_mut()
                        .unwrap()
                        .insert("seed".into(), seed.clone());
                }
                decode(&initial.sync("docInitial", args)?)
            },
            move |value, ops, info| {
                decode(&checkpoint.sync(
                    "docCheckpoint",
                    object([
                        ("id", encode(&id)),
                        ("value", checkpoint.native_value(value.value())),
                        ("ops", checkpoint.native_ops(ops)),
                        (
                            "info",
                            encode(&serde_json::json!({"deltasSinceBase":info.deltas_since_base})),
                        ),
                    ]),
                )?)
            },
        )?;
        if description["migrate"].as_bool() == Some(true) {
            let migrate = self.clone();
            definition = definition.with_migration(move |value, version| {
                decode(&migrate.sync("docMigrate", encode(&(id, value, version)))?)
            });
        }
        let definition = Arc::new(definition);
        self.documents
            .definitions
            .lock()
            .insert(id, definition.clone());
        Ok(definition)
    }
    pub(super) fn document_request(
        self: &Arc<Self>,
        method: &str,
        request: JsonValue,
        context: Context,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        if method == "doc.apply" {
            let draft = self
                .documents
                .drafts
                .lock()
                .get(&field(&request, "draft")?)
                .map(|(_, draft)| draft.clone())
                .ok_or_else(|| Error::message("Unknown native draft"))?;
            let ops: Vec<Op> = field(&request, "ops")?;
            draft.apply_ops(&ops)?;
            self.documents.writes.fetch_add(1, Ordering::Relaxed);
            return Ok(Box::pin(async { Ok(None) }));
        }
        if method == "doc.release" {
            let tx: u64 = field(&request, "tx")?;
            self.documents
                .drafts
                .lock()
                .retain(|_, (owner, _)| *owner != tx);
            return Ok(Box::pin(async { Ok(None) }));
        }
        let definition = self.document_definition(&request["definition"])?;
        let address = field(&request, "address")?;
        if method == "doc.retire" {
            let operation = self.transaction(&request)?.retire_at(&definition, address);
            return Ok(Box::pin(async move {
                operation.await?;
                Ok(None)
            }));
        }
        if method == "doc.acquire" {
            let tx: u64 = field(&request, "tx")?;
            let acquiring = self.transaction(&request)?.doc_at(
                &definition,
                address,
                request
                    .as_object()
                    .and_then(|request| request.get(&JsonString::from("seed")))
                    .cloned(),
            );
            let hub = self.clone();
            return Ok(Box::pin(async move {
                let draft = acquiring.await?;
                let value = draft.read()?;
                let mut drafts = hub.documents.drafts.lock();
                let id = if let Some((id, _)) = drafts
                    .iter()
                    .find(|(_, (owner, held))| *owner == tx && held.same_draft(&draft))
                {
                    *id
                } else {
                    let id = hub.next.fetch_add(1, Ordering::Relaxed);
                    drafts.insert(id, (tx, draft));
                    id
                };
                Ok(Some(object([
                    ("draft", encode(&id)),
                    ("value", encode(&value)),
                ])))
            }));
        }
        let runtime = request
            .as_object()
            .unwrap()
            .contains_key(&JsonString::from("runtime"))
            .then(|| self.runtime(&request))
            .transpose()?;
        match method {
            "doc.watch" => {
                self.documents.watches.fetch_add(1, Ordering::Relaxed);
                let acquisition = if let Some(runtime) = runtime {
                    runtime.watch_doc_raw(definition, address, &context)
                } else {
                    self.session(&request)?
                        .watch_raw(definition, address, &context)
                };
                let hub = self.clone();
                Ok(Box::pin(async move {
                    Ok(acquisition
                        .await?
                        .map(|watch| hub.register_watch(graph_bridge::ValueWatch::Document(watch))))
                }))
            }
            "doc.snapshot" | "doc.snapshotAsOf" => {
                self.documents.reads.fetch_add(1, Ordering::Relaxed);
                let reading = if method == "doc.snapshotAsOf" {
                    let at = field(&request, "at")?;
                    if let Some(runtime) = runtime {
                        runtime.snapshot_as_of_raw(definition, address, at, &context)
                    } else {
                        self.session(&request)?
                            .snapshot_as_of_raw(definition, address, at, &context)
                    }
                } else if let Some(runtime) = runtime {
                    runtime.snapshot_raw(definition, address, &context)
                } else {
                    self.session(&request)?
                        .snapshot_raw(definition, address, &context)
                };
                let hub = self.clone();
                Ok(Box::pin(async move {
                    Ok(reading.await?.map(|value| hub.native_value(&value)))
                }))
            }
            other => Err(Error::message(format!("Unknown document method {other}"))),
        }
    }
}
