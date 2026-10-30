//! Observations are generated only by the pinned original JS services.
use parking_lot::Mutex;
use pi_durable::{
    harness::{inbox::*, live::live_doc},
    *,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{collections::HashMap, sync::Arc};
#[derive(Deserialize)]
struct Fixture {
    reference: String,
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    name: String,
    retention: f64,
    actions: Vec<JsonValue>,
    observations: Vec<JsonValue>,
}
fn encode(value: &impl Serialize) -> JsonValue {
    pi_chord::json::to_value(value).unwrap()
}
fn decode<T: DeserializeOwned>(value: &JsonValue) -> Result<T> {
    serde_json::from_str(&value.to_string()).map_err(Error::from)
}
fn object<const N: usize>(fields: [(&str, JsonValue); N]) -> JsonValue {
    JsonValue::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect::<pi_chord::json::Map>()
            .into(),
    )
}
struct Driver {
    storage: Arc<MemoryStorage>,
    session: Session,
    service: Submissions,
    root: ConversationId,
    modes: Arc<Mutex<QueueModes>>,
    owners: HashMap<String, ConversationId>,
    handles: HashMap<String, Submission>,
}
impl Driver {
    async fn new(retention: f64) -> Self {
        let storage = Arc::new(MemoryStorage::default());
        let session = Session::new(
            storage.clone(),
            SessionOptions {
                now: Arc::new(|| 42.0),
                document_retention_ms: retention,
                ..Default::default()
            },
        );
        let modes = Arc::new(Mutex::new(QueueModes::default()));
        let reader = modes.clone();
        let service = Submissions::new(
            session.clone(),
            SubmissionsOptions {
                now: Arc::new(|| 42.0),
                queue_modes: Arc::new(move || Ok(*reader.lock())),
                resume: Arc::new(|| Ok(())),
                telemetry: pi_telemetry::noop_telemetry_context(),
            },
        )
        .unwrap();
        let root = session
            .commit(
                |tx| async move { Ok(tx.create_root_conversation().await?.id) },
                &Context::default(),
            )
            .await
            .unwrap();
        Self {
            storage,
            session,
            service,
            root,
            modes,
            owners: HashMap::from([("root".into(), root)]),
            handles: HashMap::new(),
        }
    }
    async fn run(&mut self, action: &JsonValue) -> Result<JsonValue> {
        let context = Context::default();
        let conversation = *self
            .owners
            .get(
                action["conversation"]
                    .to_utf8()
                    .as_deref()
                    .unwrap_or("root"),
            )
            .unwrap();
        let label = action["label"].to_utf8().unwrap_or_default();
        let id = if action["id"].is_null() {
            self.handles
                .get(&label)
                .map(|handle| handle.id)
                .unwrap_or_default()
        } else {
            decode(&action["id"])?
        };
        Ok(match action["op"].to_utf8().as_deref().unwrap() {
            "submit" => {
                let mut draft = action["draft"].clone();
                if draft["entry"]["head"].string_equals("first") {
                    let rows = self
                        .storage
                        .scan_entries(
                            &EntryQuery {
                                conversation_id: conversation,
                                min_entry_id: None,
                                max_entry_id: None,
                                order: None,
                            },
                            1000,
                            None,
                            &context,
                        )
                        .await?;
                    draft
                        .as_object_mut()
                        .unwrap()
                        .get_mut(&JsonString::from("entry"))
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .insert("head".into(), encode(&rows.items.last().unwrap().id));
                }
                let handle = self
                    .service
                    .submit(conversation, decode(&draft)?, &context)
                    .await?;
                let record = handle.status(&context).await?;
                self.handles.insert(label, handle);
                encode(&record)
            }
            "get" => encode(
                &self
                    .service
                    .get(id, &context)
                    .await?
                    .map(|handle| handle.id),
            ),
            "status" => encode(&self.service.status(id, &context).await?),
            "abort" => self
                .service
                .abort(id, &context, decode(&action["conversationId"])?)
                .await?
                .map(|result| encode(&result))
                .unwrap_or_else(|| "not_found".into()),
            "modes" => {
                let mode = |value: &JsonValue| {
                    if value.string_equals("all") {
                        QueueMode::All
                    } else {
                        QueueMode::OneAtATime
                    }
                };
                *self.modes.lock() = QueueModes {
                    steering_mode: mode(&action["steeringMode"]),
                    follow_up_mode: mode(&action["followUpMode"]),
                };
                JsonValue::Null
            }
            "clearRun" => {
                self.session
                    .commit(
                        move |tx| async move {
                            let draft = tx.doc(&live_doc(), conversation).await?;
                            draft.apply_ops(&[pi_chord::delta::Op::Delete(vec!["run".into()])])
                        },
                        &context,
                    )
                    .await?;
                JsonValue::Null
            }
            "withdraw" => {
                let reason = decode::<JsonString>(&action["reason"])?;
                self.session.commit(move |tx|async move {withdraw_queued_inputs(&tx,conversation,reason).await},&context).await?;
                JsonValue::Null
            }
            "boundary" => {
                let modes = *self.modes.lock();
                let at = if action["at"].string_equals("postTools") {
                    BoundaryAt::PostTools
                } else {
                    BoundaryAt::Final
                };
                let result = self
                    .session
                    .commit(
                        move |tx| async move {
                            let mut boundary = prepare_boundary(&tx, conversation, modes).await?;
                            apply_boundary(&tx, &mut boundary, at, 42.0).await
                        },
                        &context,
                    )
                    .await?;
                object([
                    ("users", encode(&result.users)),
                    ("reset", encode(&result.reset)),
                ])
            }
            "conversation" => {
                let id = self
                    .session
                    .commit(
                        |tx| async move {
                            Ok(tx
                                .create_conversation(ConversationOwnership::Ownerless)
                                .await?
                                .id)
                        },
                        &context,
                    )
                    .await?;
                self.owners.insert(label, id);
                encode(&id)
            }
            other => return Err(Error::message(format!("Unknown fixture operation {other}"))),
        })
    }
    async fn dump(&self) -> JsonValue {
        let context = Context::default();
        object([
            (
                "live",
                encode(
                    &self
                        .session
                        .snapshot(&live_doc(), self.root, &context)
                        .await
                        .unwrap(),
                ),
            ),
            (
                "inbox",
                encode(
                    &self
                        .session
                        .snapshot(&inbox_doc(), self.root, &context)
                        .await
                        .unwrap(),
                ),
            ),
            (
                "entries",
                encode(
                    &self
                        .storage
                        .scan_entries(
                            &EntryQuery {
                                conversation_id: self.root,
                                min_entry_id: None,
                                max_entry_id: None,
                                order: None,
                            },
                            1000,
                            None,
                            &context,
                        )
                        .await
                        .unwrap()
                        .items,
                ),
            ),
            (
                "tasks",
                encode(
                    &self
                        .storage
                        .scan_tasks(&TaskQuery::default(), 1000, None, &context)
                        .await
                        .unwrap()
                        .items,
                ),
            ),
            (
                "submissions",
                encode(
                    &self
                        .storage
                        .scan_submissions(&SubmissionQuery::default(), 1000, None, &context)
                        .await
                        .unwrap()
                        .items,
                ),
            ),
        ])
    }
}
#[tokio::test]
async fn native_submissions_match_every_independent_reference_observation() {
    let fixture: Fixture = serde_json::from_str(include_str!("fixtures/submissions.json")).unwrap();
    assert_eq!(
        fixture.reference,
        "5e63241d0294ed3fcfc4388c737a8ea27e09c74e"
    );
    assert_eq!(fixture.cases.len(), 28);
    let mut count = 0;
    for case in fixture.cases {
        let mut driver = Driver::new(case.retention).await;
        assert_eq!(case.actions.len(), case.observations.len());
        for (index, (action, expected)) in case.actions.iter().zip(case.observations).enumerate() {
            let result = match driver.run(action).await {
                Ok(value) => object([("value", value)]),
                Err(error) => {
                    let mut fields = object([
                        ("name", encode(&error.name())),
                        ("message", encode(&error.to_string())),
                    ]);
                    if let ErrorKind::ConversationBusy(id) = error.kind() {
                        fields
                            .as_object_mut()
                            .unwrap()
                            .insert("conversationId".into(), encode(id));
                    }
                    object([("error", fields)])
                }
            };
            let observed = object([("result", result), ("state", driver.dump().await)]);
            assert_eq!(
                observed, expected,
                "{} retention={} step={index}, action={action}",
                case.name, case.retention
            );
            count += 1;
        }
        driver.session.close(&Context::default()).await.unwrap();
    }
    assert_eq!(count, 198);
}
