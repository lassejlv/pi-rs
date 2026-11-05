//! Writes, returned values, error names and complete publications are generated
//! independently by the pinned TypeScript Session in both retention modes.
#[allow(dead_code)]
#[path = "support/session_storage.rs"]
mod support;
use parking_lot::Mutex;
use pi_durable::*;
use serde::Serialize;
use serde_json::Value as Spec;
use std::{collections::HashMap, sync::Arc};

fn encode<T: Serialize>(value: &T) -> JsonValue {
    pi_chord::json::to_value(value).unwrap()
}
fn definition(spec: &Spec) -> Arc<AnyDocDefinition> {
    fn scoped<S: DocScope>(spec: &Spec, scope: S) -> Arc<AnyDocDefinition> {
        let threshold = spec["checkpointAfter"].as_u64();
        let checkpoint = move |_: &JsonObject, _: &[pi_chord::delta::Op], info: CheckpointInfo| {
            threshold.is_some_and(|t| info.deltas_since_base >= t as usize)
        };
        let kind = spec["kind"].as_str().unwrap();
        let version = spec["version"].as_u64().unwrap();
        if spec["family"] == true {
            define_doc_family(DocFamilyDefinition::new(
                kind,
                version,
                scope,
                |seed: JsonObject| Ok(seed),
                checkpoint,
            ))
            .unwrap()
            .definition()
            .clone()
        } else {
            let mut definition = DocDefinition::new(
                kind,
                version,
                scope,
                || Ok(serde_json::from_str::<JsonObject>("{\"count\":0}")?),
                checkpoint,
            );
            if spec["migrate"] == true {
                definition = definition.with_migration(|mut value, _| {
                    value.insert("label".into(), "migrated".into());
                    Ok(value)
                });
            }
            define_doc(definition).unwrap().definition().clone()
        }
    }
    match spec["scope"].as_str().unwrap() {
        "session" => scoped(spec, SessionScope),
        "task" => scoped(spec, TaskScope),
        "conversation" if spec["history"] == "rewindable" => scoped(
            spec,
            RewindableConversationScope {
                fork: ForkPolicy::AsOf,
            },
        ),
        "conversation" => scoped(
            spec,
            LatestConversationScope {
                fork: if spec["fork"] == "current" {
                    LatestFork::Current
                } else {
                    LatestFork::Initial
                },
            },
        ),
        _ => panic!("invalid scope"),
    }
}
type Env = Arc<Mutex<HashMap<String, i64>>>;
fn id(env: &Env, step: &Spec, field: &str) -> i64 {
    env.lock()[step[field].as_str().unwrap()]
}
fn address(definition: &AnyDocDefinition, step: &Spec, env: &Env) -> DocumentAddress {
    DocumentAddress {
        kind: definition.kind().clone(),
        key: step["key"].as_str().map(Into::into),
        scope: match definition.scope() {
            DefinitionScope::Session => DocumentScope::Session,
            DefinitionScope::Task => DocumentScope::Task {
                task_id: TaskId::from_raw(id(env, step, "owner")),
            },
            DefinitionScope::Conversation { .. } => DocumentScope::Conversation {
                conversation_id: ConversationId::from_raw(id(env, step, "owner")),
            },
        },
    }
}
async fn replay(index: usize) {
    let fixture: Spec = serde_json::from_str(include_str!("fixtures/session.json")).unwrap();
    let case = &fixture["cases"][index];
    let definitions: Arc<HashMap<_, _>> = Arc::new(
        fixture["definitions"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, spec)| (name.clone(), definition(spec)))
            .collect(),
    );
    let env: Env = Arc::default();
    let storage = Arc::new(support::ControlledStorage::default());
    let session = Session::new(
        storage.clone(),
        SessionOptions {
            now: Arc::new(|| 1234.0),
            document_retention_ms: case["retention"].as_f64().unwrap_or(f64::INFINITY),
            set_timer: Some(Arc::new(|_, _, _| None)),
            report: Arc::new(|_| {}),
            ..Default::default()
        },
    );
    let publications = Arc::new(Mutex::new(Vec::new()));
    let capture = publications.clone();
    let _subscription = session.subscribe_commits(move |p, _| {
        capture.lock().push(encode(p));
        Ok(())
    });
    let mut records = Vec::new();
    for step in case["steps"].as_array().unwrap() {
        let result = if step["op"] == "snapshot" {
            let definition = &definitions[step["token"].as_str().unwrap()];
            session
                .snapshot_raw(
                    definition.clone(),
                    address(definition, step, &env),
                    &Context::default(),
                )
                .await
                .map(|v| v.map_or(JsonValue::Null, |v| v.as_ref().clone()))
        } else {
            let (step, env, definitions, storage) = (
                step.clone(),
                env.clone(),
                definitions.clone(),
                storage.clone(),
            );
            session
                .commit(
                    move |tx| async move {
                        let mut values = Vec::new();
                        for action in step["steps"].as_array().unwrap() {
                            let (value, save) = match action["op"].as_str().unwrap() {
                                "conversation" => {
                                    let ownership = if action["task"].is_string() {
                                        ConversationOwnership::Task(TaskId::from_raw(id(
                                            &env, action, "task",
                                        )))
                                    } else {
                                        ConversationOwnership::Ownerless
                                    };
                                    let record = tx.create_conversation(ownership).await?;
                                    (encode(&record), Some(record.id.get()))
                                }
                                "fork" => {
                                    let record = tx
                                        .fork_conversation(
                                            ConversationId::from_raw(id(&env, action, "parent")),
                                            EntryId::from_raw(id(&env, action, "at")),
                                            ConversationOwnership::Ownerless,
                                        )
                                        .await?;
                                    (encode(&record), Some(record.id.get()))
                                }
                                "entry" => {
                                    let mut draft = EntryDraft::new("note");
                                    draft.head = Some(EntryHead::ThisEntry);
                                    let record = tx
                                        .append_entry(
                                            ConversationId::from_raw(id(&env, action, "owner")),
                                            draft,
                                        )
                                        .await?;
                                    (encode(&record), Some(record.id.get()))
                                }
                                "doc" => {
                                    let definition =
                                        &definitions[action["token"].as_str().unwrap()];
                                    let seed = action.get("seed").map(encode);
                                    let draft = tx
                                        .doc_at(definition, address(definition, action, &env), seed)
                                        .await?;
                                    if let Some(count) = action["count"].as_u64() {
                                        draft.update(|v| {
                                            v.insert("count".into(), encode(&count));
                                            Ok(())
                                        })?;
                                    }
                                    (encode(&draft.read()?), None)
                                }
                                "retire" => {
                                    let definition =
                                        &definitions[action["token"].as_str().unwrap()];
                                    tx.retire_at(definition, address(definition, action, &env))
                                        .await?;
                                    (JsonValue::Null, None)
                                }
                                "task" => {
                                    let mut options = TaskOptions::new(TaskOwnership::Conversation);
                                    options.conversation_id =
                                        Some(ConversationId::from_raw(id(&env, action, "owner")));
                                    options.trace = Some(TaskTrace::Root);
                                    let id = tx
                                        .create_task_record(
                                            "test".into(),
                                            1,
                                            JsonValue::Null,
                                            |_| {
                                                Ok(pi_chord::json::parse("{\"phase\":\"start\"}")
                                                    .unwrap())
                                            },
                                            options,
                                        )
                                        .await?;
                                    (encode(&id), Some(id.get()))
                                }
                                "submission" => {
                                    let record = tx
                                        .create_submission(SubmissionCreate {
                                            conversation_id: ConversationId::from_raw(id(
                                                &env, action, "owner",
                                            )),
                                            trace: None,
                                            request_id: None,
                                            state: SubmissionState::InputQueued,
                                        })
                                        .await?;
                                    (encode(&record), Some(record.id().get()))
                                }
                                "place" => {
                                    tx.place_submission(
                                        SubmissionId::from_raw(id(&env, action, "submission")),
                                        EntryId::from_raw(id(&env, action, "entry")),
                                    )?;
                                    (JsonValue::Null, None)
                                }
                                "settle" => {
                                    tx.settle_submission(
                                        SubmissionId::from_raw(id(&env, action, "submission")),
                                        SubmissionSettlement::Done {
                                            answer: EntryId::from_raw(id(&env, action, "answer")),
                                        },
                                    )?;
                                    (JsonValue::Null, None)
                                }
                                "terminal" => {
                                    let task_id = TaskId::from_raw(id(&env, action, "task"));
                                    let staged =
                                        tx.staged_tasks().into_iter().find(|t| t.id() == task_id);
                                    let mut record = match staged {
                                        Some(record) => record,
                                        None => storage
                                            .task(task_id, &Context::default())
                                            .await?
                                            .unwrap(),
                                    };
                                    record.state = TaskState::Terminal {
                                        outcome: TaskOutcome::Completed {
                                            result: JsonValue::Null,
                                        },
                                    };
                                    tx.set_task(record)?;
                                    (JsonValue::Null, None)
                                }
                                other => panic!("Unknown {other}"),
                            };
                            if let Some(name) = action["save"].as_str() {
                                env.lock().insert(name.into(), save.unwrap());
                            }
                            values.push(value);
                        }
                        if step["fail"] == true {
                            return Err(Error::message("callback failed"));
                        }
                        Ok(JsonValue::Array(values.into()))
                    },
                    &Context::default(),
                )
                .await
        };
        let record = match result {
            Ok(value) => {
                let mut map = pi_chord::json::Map::new();
                map.insert("value".into(), value);
                JsonValue::Object(map.into())
            }
            Err(error) => encode(
                &serde_json::json!({"error":{"name":error.name(),"message":error.to_string()}}),
            ),
        };
        records.push(record);
    }
    session.close(&Context::default()).await.unwrap();
    assert_eq!(
        encode(&records),
        encode(&case["records"]),
        "{} returned results",
        case["name"]
    );
    assert_eq!(
        encode(&*storage.writes.lock()),
        encode(&case["writes"]),
        "{} storage writes",
        case["name"]
    );
    assert_eq!(
        encode(&*publications.lock()),
        encode(&case["publications"]),
        "{} publications",
        case["name"]
    );
}
macro_rules! case {
    ($name:ident,$index:expr) => {
        #[tokio::test]
        async fn $name() {
            replay($index).await;
        }
    };
}
case!(creation_zero, 0);
case!(migration_zero, 1);
case!(checkpoints_zero, 2);
case!(families_zero, 3);
case!(forks_zero, 4);
case!(fork_validation_zero, 5);
case!(tasks_submissions_zero, 6);
case!(owner_validation_zero, 7);
case!(creation_infinite, 8);
case!(migration_infinite, 9);
case!(checkpoints_infinite, 10);
case!(families_infinite, 11);
case!(forks_infinite, 12);
case!(fork_validation_infinite, 13);
case!(tasks_submissions_infinite, 14);
case!(owner_validation_infinite, 15);
