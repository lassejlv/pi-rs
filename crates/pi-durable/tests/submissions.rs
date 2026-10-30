#[path = "support/session_storage.rs"]
mod support;
use parking_lot::Mutex;
use pi_chord::{
    AbortController,
    context::{AbortError, AbortReason, with_abort_signal},
};
use pi_durable::{
    harness::{generation::*, inbox::*, live::*, submissions::*, usage::*},
    *,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::{ControlledStorage, Gate};

fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
fn object(text: &str) -> JsonObject {
    JsonObject::try_from_value(json(text)).unwrap()
}
struct Opened {
    storage: Arc<ControlledStorage>,
    session: Session,
    service: Submissions,
    root: ConversationId,
    resumes: Arc<AtomicUsize>,
}
async fn open() -> Opened {
    open_with(Arc::new(|| Ok(QueueModes::default()))).await
}
async fn open_with(modes: Arc<dyn Fn() -> Result<QueueModes> + Send + Sync>) -> Opened {
    let storage = Arc::new(ControlledStorage::default());
    let session = Session::new(
        storage.clone(),
        SessionOptions {
            now: Arc::new(|| 42.0),
            conversation_created: Some(Arc::new(|tx, record| {
                Box::pin(async move { harness::create_conversation_documents(&tx, record).await })
            })),
            ..Default::default()
        },
    );
    let resumes = Arc::new(AtomicUsize::new(0));
    let resume = resumes.clone();
    let service = Submissions::new(
        session.clone(),
        SubmissionsOptions {
            now: Arc::new(|| 42.0),
            queue_modes: modes,
            resume: Arc::new(move || {
                resume.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
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
    Opened {
        storage,
        session,
        service,
        root,
        resumes,
    }
}
fn input(content: &str, mode: Option<WhenBusy>, request: Option<&str>) -> SubmissionDraft {
    SubmissionDraft::Input {
        content: content.into(),
        when_busy: mode,
        entry: None,
        request_id: request.map(Into::into),
    }
}
async fn submit(opened: &Opened, draft: SubmissionDraft) -> Submission {
    opened
        .service
        .submit(opened.root, draft, &Context::default())
        .await
        .unwrap()
}
async fn entries(opened: &Opened) -> Vec<EntryRecord> {
    opened
        .storage
        .memory
        .scan_entries(
            &EntryQuery {
                conversation_id: opened.root,
                min_entry_id: None,
                max_entry_id: None,
                order: Some(ScanOrder::Ascending),
            },
            100,
            None,
            &Context::default(),
        )
        .await
        .unwrap()
        .items
}

#[tokio::test]
async fn admission_places_input_deduplicates_before_writes_and_rejects_busy_without_a_commit() {
    let opened = open().await;
    let first = submit(&opened, input("hello", None, Some("request"))).await;
    let record = first.status(&Context::default()).await.unwrap();
    let SubmissionState::InputPlaced { entry } = record.state else {
        panic!("not placed")
    };
    let placed = opened
        .storage
        .memory
        .entry(entry, &Context::default())
        .await
        .unwrap()
        .unwrap()
        .entry;
    assert_eq!(
        placed.model.unwrap(),
        vec![json(r#"{"role":"user","content":"hello","timestamp":42}"#)]
    );
    let live = opened
        .session
        .snapshot(&live_doc(), opened.root, &Context::default())
        .await
        .unwrap()
        .unwrap();
    let run = live.run.as_ref().unwrap();
    assert_eq!(run.inputs, vec![first.id]);
    let task = opened
        .storage
        .memory
        .task(run.task_id, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.info.kind, "pi.generation".into());
    assert_eq!(task.info.input, json("{}"));
    assert_eq!(
        task.state.checkpoint(),
        Some(&json(r#"{"phase":"prepare","attempt":1}"#))
    );
    let commits = opened.storage.writes.lock().len();
    let duplicate = submit(
        &opened,
        input("different", Some(WhenBusy::Reject), Some("request")),
    )
    .await;
    assert_eq!(duplicate.id, first.id);
    assert_eq!(opened.storage.writes.lock().len(), commits);
    let rejected = opened
        .service
        .submit(
            opened.root,
            input("no", Some(WhenBusy::Reject), None),
            &Context::default(),
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(rejected.kind(),ErrorKind::ConversationBusy(id) if *id==opened.root));
    assert_eq!(opened.storage.writes.lock().len(), commits);
    let conflict = SubmissionDraft::Write {
        request_id: Some("request".into()),
        entry: EntryDraft::new("note"),
    };
    assert!(
        opened
            .service
            .submit(opened.root, conflict, &Context::default())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("already identifies a submission of type input")
    );
    let invalid = SubmissionDraft::Input {
        request_id: Some("request".into()),
        content: "bad".into(),
        when_busy: None,
        entry: Some(InputEntry::new("pi.custom")),
    };
    assert_eq!(
        opened
            .service
            .submit(opened.root, invalid, &Context::default())
            .await
            .err()
            .unwrap()
            .name(),
        "TypeError"
    );
    assert!(opened.resumes.load(Ordering::SeqCst) >= 5);
    opened.session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn boundaries_place_resets_before_users_mark_old_heads_stale_and_keep_unselected_items() {
    let opened = open().await;
    let first = submit(&opened, input("a", None, None)).await;
    let SubmissionState::InputPlaced { entry: old_head } =
        first.status(&Context::default()).await.unwrap().state
    else {
        panic!()
    };
    let follow = submit(&opened, input("f1", None, None)).await;
    let remaining = submit(&opened, input("f2", None, None)).await;
    let mut reset = EntryDraft::new("pi.reset");
    reset.head = Some(EntryHead::ThisEntry);
    let reset = submit(&opened, SubmissionDraft::write(reset)).await;
    let mut stale = EntryDraft::new("old");
    stale.head = Some(EntryHead::Entry(old_head));
    let stale = submit(&opened, SubmissionDraft::write(stale)).await;
    let mut note = EntryDraft::new("note");
    note.data = Some(JsonValue::Null);
    let note = submit(&opened, SubmissionDraft::write(note)).await;
    assert_eq!(
        note.status(&Context::default())
            .await
            .unwrap()
            .state
            .status(),
        SubmissionStatus::Queued
    );
    let steer = submit(&opened, input("s1", Some(WhenBusy::Steer), None)).await;
    let root = opened.root;
    let result = opened
        .session
        .commit(
            move |tx| async move {
                let mut boundary = prepare_boundary(&tx, root, QueueModes::default()).await?;
                apply_boundary(&tx, &mut boundary, BoundaryAt::PostTools, 100.0).await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    assert!(result.reset);
    assert_eq!(result.users, vec![follow.id, steer.id]);
    let rows = entries(&opened).await;
    assert_eq!(
        rows.iter()
            .map(|entry| entry.kind.to_string_lossy())
            .collect::<Vec<_>>(),
        ["pi.user", "pi.reset", "note", "pi.user", "pi.user"]
    );
    assert_eq!(rows[2].data, Some(JsonValue::Null));
    assert_eq!(rows[1].head, Some(rows[1].id));
    assert!(
        matches!(stale.wait(&Context::default()).await.unwrap().state,SubmissionState::WriteUnanswered {ref reason,..} if reason.equals_str("stale"))
    );
    for handle in [&reset, &note] {
        assert_eq!(
            handle
                .wait(&Context::default())
                .await
                .unwrap()
                .state
                .status(),
            SubmissionStatus::Done
        );
    }
    let inbox = opened
        .session
        .snapshot(&inbox_doc(), root, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbox.items.len(), 1);
    assert_eq!(inbox.items[0].id(), remaining.id);
    opened.session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn queue_modes_select_each_mode_independently_at_both_boundaries() {
    for steering in [QueueMode::All, QueueMode::OneAtATime] {
        for follow_up in [QueueMode::All, QueueMode::OneAtATime] {
            let opened = open().await;
            submit(&opened, input("active", None, None)).await;
            let mut steers = Vec::new();
            let mut follows = Vec::new();
            for n in 0..3 {
                steers.push(
                    submit(
                        &opened,
                        input(&format!("s{n}"), Some(WhenBusy::Steer), None),
                    )
                    .await
                    .id,
                );
                follows.push(
                    submit(&opened, input(&format!("f{n}"), None, None))
                        .await
                        .id,
                );
            }
            let root = opened.root;
            let modes = QueueModes {
                steering_mode: steering,
                follow_up_mode: follow_up,
            };
            let post = opened
                .session
                .commit(
                    move |tx| async move {
                        let mut boundary = prepare_boundary(&tx, root, modes).await?;
                        apply_boundary(&tx, &mut boundary, BoundaryAt::PostTools, 42.0).await
                    },
                    &Context::default(),
                )
                .await
                .unwrap();
            let expected = if steering == QueueMode::All {
                steers.clone()
            } else {
                steers[..1].to_vec()
            };
            assert_eq!(post.users, expected);
            assert!(!post.reset);
            let final_result = opened
                .session
                .commit(
                    move |tx| async move {
                        let mut boundary = prepare_boundary(&tx, root, modes).await?;
                        apply_boundary(&tx, &mut boundary, BoundaryAt::Final, 42.0).await
                    },
                    &Context::default(),
                )
                .await
                .unwrap();
            let mut expected = if steering == QueueMode::All {
                vec![]
            } else {
                steers[1..2].to_vec()
            };
            expected.extend(if follow_up == QueueMode::All {
                follows.clone()
            } else {
                follows[..1].to_vec()
            });
            expected.sort_unstable();
            assert_eq!(final_result.users, expected);
            opened.session.close(&Context::default()).await.unwrap();
        }
    }
}

#[tokio::test]
async fn display_writes_bypass_busy_and_withdrawal_removes_only_queued_inputs() {
    let opened = open().await;
    let active = submit(&opened, input("active", None, None)).await;
    let a = submit(&opened, input("a", None, None)).await;
    let b = submit(&opened, input("b", Some(WhenBusy::Steer), None)).await;
    let display = submit(&opened, SubmissionDraft::write(EntryDraft::new("display"))).await;
    assert_eq!(
        display
            .wait(&Context::default())
            .await
            .unwrap()
            .state
            .status(),
        SubmissionStatus::Done
    );
    let mut entry = EntryDraft::new("queued");
    entry.model = Some(vec![]);
    let write = submit(&opened, SubmissionDraft::write(entry)).await;
    assert_eq!(
        opened
            .service
            .abort(
                a.id,
                &Context::default(),
                Some(ConversationId::from_raw(999))
            )
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        active.abort(&Context::default()).await.unwrap(),
        SubmissionAbort::AlreadyPlaced
    );
    assert_eq!(
        a.abort(&Context::default()).await.unwrap(),
        SubmissionAbort::Aborted
    );
    assert_eq!(
        a.abort(&Context::default()).await.unwrap(),
        SubmissionAbort::Settled
    );
    let root = opened.root;
    opened
        .session
        .commit(
            move |tx| async move { withdraw_queued_inputs(&tx, root, "run_failed").await },
            &Context::default(),
        )
        .await
        .unwrap();
    assert!(
        matches!(b.wait(&Context::default()).await.unwrap().state,SubmissionState::InputUnanswered {ref reason,..} if reason.equals_str("run_failed"))
    );
    let inbox = opened
        .session
        .snapshot(&inbox_doc(), root, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbox.items.len(), 1);
    assert_eq!(inbox.items[0].id(), write.id);
    assert!(
        opened
            .service
            .get(SubmissionId::from_raw(999), &Context::default())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        opened
            .service
            .status(SubmissionId::from_raw(999), &Context::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("does not exist")
    );
    assert_eq!(
        opened
            .service
            .abort(SubmissionId::from_raw(999), &Context::default(), None)
            .await
            .unwrap(),
        None
    );
    opened.session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn wait_cancellation_is_local_and_close_spanning_the_read_rejects_registration() {
    let opened = open().await;
    let active = submit(&opened, input("active", None, None)).await;
    let cancelled = AbortController::new();
    let waiter = active.wait(&with_abort_signal(cancelled.signal(), &Context::default()));
    cancelled.abort_with_reason(AbortReason::Error(AbortError::new("stop waiting")));
    assert_eq!(waiter.await.unwrap_err().to_string(), "stop waiting");
    assert_eq!(
        active
            .status(&Context::default())
            .await
            .unwrap()
            .state
            .status(),
        SubmissionStatus::Placed
    );
    let gate = Arc::new(Gate::default());
    *opened.storage.submission_gate.lock() = Some(gate.clone());
    let waiting = active.wait(&Context::default());
    gate.entered().await;
    let close = opened.session.close(&Context::default());
    gate.release();
    assert!(waiting.await.unwrap_err().to_string().contains("closed"));
    close.await.unwrap();
}

#[tokio::test]
async fn dropped_admission_waiters_do_not_cancel_atomic_input_task_and_live_publication() {
    let opened = open().await;
    let gate = Arc::new(Gate::default());
    *opened.storage.commit_gate.lock() = Some(gate.clone());
    let admitted = opened.service.submit(
        opened.root,
        input("durable", None, None),
        &Context::default(),
    );
    gate.entered().await;
    let writes = opened.storage.writes.lock().last().unwrap().clone();
    let submission = writes
        .iter()
        .find_map(|write| {
            if let StorageWrite::Submission { value } = write {
                Some(value.id())
            } else {
                None
            }
        })
        .unwrap();
    let task = writes
        .iter()
        .find_map(|write| {
            if let StorageWrite::Task { value } = write {
                Some(value.id())
            } else {
                None
            }
        })
        .unwrap();
    assert!(
        opened
            .storage
            .memory
            .submission(submission, &Context::default())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        opened
            .storage
            .memory
            .task(task, &Context::default())
            .await
            .unwrap()
            .is_none()
    );
    drop(admitted);
    gate.release();
    let handle = opened
        .service
        .get(submission, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        handle
            .status(&Context::default())
            .await
            .unwrap()
            .state
            .status(),
        SubmissionStatus::Placed
    );
    let live = opened
        .session
        .snapshot(&live_doc(), opened.root, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(live.run.as_ref().unwrap().task_id, task);
    assert_eq!(live.run.as_ref().unwrap().inputs, vec![submission]);
    opened.session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn idle_admission_reads_live_queue_modes_on_the_line_and_hands_over_run_ownership() {
    let modes = Arc::new(Mutex::new(QueueModes::default()));
    let reader = modes.clone();
    let opened = open_with(Arc::new(move || Ok(*reader.lock()))).await;
    let active = submit(&opened, input("active", None, None)).await;
    let first = submit(&opened, input("first", None, None)).await;
    let second = submit(&opened, input("second", None, None)).await;
    let root = opened.root;
    opened
        .session
        .commit(
            move |tx| async move {
                let live = tx.doc(&live_doc(), root).await?;
                live.apply_ops(&[pi_chord::delta::Op::Delete(vec!["run".into()])])
            },
            &Context::default(),
        )
        .await
        .unwrap();
    *modes.lock() = QueueModes {
        steering_mode: QueueMode::All,
        follow_up_mode: QueueMode::All,
    };
    let third = submit(&opened, input("third", None, None)).await;
    let live = opened
        .session
        .snapshot(&live_doc(), root, &Context::default())
        .await
        .unwrap()
        .unwrap();
    let run = live.run.as_ref().unwrap();
    assert_eq!(run.inputs, vec![first.id, second.id, third.id]);
    assert_eq!(
        active
            .status(&Context::default())
            .await
            .unwrap()
            .state
            .status(),
        SubmissionStatus::Placed
    );
    let from = run.task_id;
    let to = opened
        .session
        .commit(
            move |tx| async move {
                let live = tx.doc(&live_doc(), root).await?;
                let to = create_generation(&tx, root, None).await?;
                hand_over(&live, TaskId::from_raw(-99), to)?;
                assert_eq!(live.read()?.run.unwrap().task_id, from);
                hand_over(&live, from, to)?;
                Ok(to)
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let live = opened
        .session
        .snapshot(&live_doc(), root, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(live.run.as_ref().unwrap().task_id, to);
    assert_eq!(
        live.run.as_ref().unwrap().inputs,
        vec![first.id, second.id, third.id]
    );
    opened.session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn scheduler_fault_cleanup_preserves_partial_spend_and_settles_only_the_owned_run() {
    let opened = open().await;
    let active = submit(&opened, input("active", None, None)).await;
    let live = opened
        .session
        .snapshot(&live_doc(), opened.root, &Context::default())
        .await
        .unwrap()
        .unwrap();
    let task = live.run.as_ref().unwrap().task_id;
    let usage = object(
        r#"{"input":2,"output":3,"cacheRead":1,"cacheWrite":0,"totalTokens":6,"reasoning":1,"cost":{"input":0.1,"output":0.2,"cacheRead":0,"cacheWrite":0,"total":0.3}}"#,
    );
    let mut message=object(r#"{"role":"assistant","content":[{"type":"text","text":"partial"}],"api":"faux","provider":"test","model":"model","stopReason":"pending","timestamp":42}"#).into_value();
    message
        .as_object_mut()
        .unwrap()
        .insert("usage".into(), pi_chord::json::to_value(&usage).unwrap());
    let root = opened.root;
    opened
        .session
        .commit(
            move |tx| async move {
                let live = tx.doc(&live_doc(), root).await?;
                live.update(|state| {
                    state.generation = Some(LiveGeneration {
                        attempt: 1,
                        message: Some(JsonObject::try_from_value(message).unwrap()),
                        retry: None,
                        deferred: None,
                    });
                    Ok(())
                })
            },
            &Context::default(),
        )
        .await
        .unwrap();
    opened
        .session
        .commit(
            move |tx| async move {
                let record = tx.task(task).await?.unwrap();
                let outcome = TaskOutcome::Faulted {
                    error: TaskOutcomeError {
                        message: "phase bug".into(),
                        detail: None,
                    },
                };
                let mut terminal = record.clone();
                terminal.state = TaskState::Terminal {
                    outcome: outcome.clone(),
                };
                tx.set_task(terminal)?;
                settle_scheduler_outcome(tx, record, outcome).await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let receipt = active.wait(&Context::default()).await.unwrap();
    assert!(
        matches!(receipt.state,SubmissionState::InputUnanswered {ref reason,ref detail,..} if reason.equals_str("faulted")&&*detail==Some(JsonValue::from("phase bug")))
    );
    let rows = entries(&opened).await;
    let partial = rows.last().unwrap();
    assert!(partial.kind.equals_str("pi.assistant"));
    assert!(partial.by_task_id.is_none());
    assert!(partial.model.as_ref().unwrap()[0]["stopReason"].string_equals("aborted"));
    let ledger = opened
        .session
        .snapshot(&usage_doc(), root, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ledger.models.get(&JsonString::from("test/model")),
        Some(&usage)
    );
    let live = opened
        .session
        .snapshot(&live_doc(), root, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert!(live.run.is_none() && live.generation.is_none());
    opened.session.close(&Context::default()).await.unwrap();
}

#[test]
fn usage_addition_keeps_fractional_and_optional_counters_and_own_property_names() {
    let mut a = object(
        r#"{"input":1.5,"output":2,"cacheRead":0,"cacheWrite":1,"cacheWrite1h":1,"totalTokens":4.5,"cost":{"input":1,"output":2,"cacheRead":0,"cacheWrite":1,"total":4}}"#,
    );
    let b = object(
        r#"{"input":0.5,"output":3,"cacheRead":2,"cacheWrite":0,"totalTokens":5.5,"reasoning":2,"cost":{"input":0.5,"output":1,"cacheRead":1,"cacheWrite":0,"total":2.5}}"#,
    );
    add_usage(&mut a, &b).unwrap();
    assert_eq!(
        a,
        object(
            r#"{"input":2,"output":5,"cacheRead":2,"cacheWrite":1,"cacheWrite1h":1,"totalTokens":10,"reasoning":2,"cost":{"input":1.5,"output":3,"cacheRead":1,"cacheWrite":1,"total":6.5}}"#
        )
    );
    let mut total = UsageState::default();
    total.tools.insert("__proto__".into(), a.clone());
    let mut state = UsageState::default();
    state.tools.insert("__proto__".into(), b.clone());
    state.tools.insert("toString".into(), b.clone());
    add_usage_state(&mut total, &state).unwrap();
    assert_eq!(total.tools.get(&JsonString::from("toString")), Some(&b));
    assert_eq!(
        total
            .tools
            .get(&JsonString::from("__proto__"))
            .unwrap()
            .get(&JsonString::from("input")),
        Some(&json("2.5"))
    );
}

#[tokio::test]
async fn provider_identity_is_stable_and_forks_initialize_their_own_builtin_documents() {
    let opened = open().await;
    let root = opened.root;
    let before = opened
        .session
        .snapshot(&provider_doc(), root, &Context::default())
        .await
        .unwrap()
        .unwrap();
    let at = opened
        .session
        .commit(
            move |tx| async move {
                Ok(tx
                    .append_entry(root, EntryDraft::new("fork-point"))
                    .await?
                    .id)
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let child = opened
        .session
        .commit(
            move |tx| async move {
                Ok(tx
                    .fork_conversation(root, at, ConversationOwnership::Ownerless)
                    .await?
                    .id)
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let after = opened
        .session
        .snapshot(&provider_doc(), root, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&before, &after));
    let child_provider = opened
        .session
        .snapshot(&provider_doc(), child, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(before.session_id, child_provider.session_id);
    assert!(
        opened
            .session
            .snapshot(&inbox_doc(), child, &Context::default())
            .await
            .unwrap()
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        opened
            .session
            .snapshot(&live_doc(), child, &Context::default())
            .await
            .unwrap()
            .unwrap()
            .run
            .is_none()
    );
    assert!(
        opened
            .session
            .snapshot(&usage_doc(), child, &Context::default())
            .await
            .unwrap()
            .unwrap()
            .models
            .is_empty()
    );
    opened.session.close(&Context::default()).await.unwrap();
}

#[test]
fn wire_records_preserve_explicit_nulls_and_lone_utf16_units() {
    fn round_trip<T: serde::Serialize + serde::de::DeserializeOwned>(source: &str) {
        let value: T = serde_json::from_str(source).unwrap();
        assert_eq!(pi_chord::json::to_value(&value).unwrap(), json(source));
    }
    round_trip::<EntryDraft>(
        r#"{"kind":"\ud800","data":null,"head":"self","edits":[],"model":[{"content":"\udc00"}]}"#,
    );
    round_trip::<SubmissionCreate>(
        r#"{"type":"write","status":"unanswered","conversationId":1,"requestId":"\ud800","reason":"old","detail":null}"#,
    );
    round_trip::<SubmissionSettlement>(
        r#"{"status":"unanswered","reason":"\ud800","detail":null}"#,
    );
    round_trip::<SubmissionDraft>(
        r#"{"type":"input","content":[{"type":"text","text":"\ud800"}],"entry":{"kind":"app.input","data":null}}"#,
    );
    round_trip::<LiveState>(
        r#"{"tools":[{"callId":"\ud800","name":"tool","status":"running","details":null}],"generation":{"attempt":1,"message":{"content":"\ud800"}}}"#,
    );
}

#[test]
fn live_slot_and_compaction_helpers_preserve_parent_order_and_release_progress() {
    let nested = |id, parent| NestedToolSlot {
        progress: SlotProgress {
            status: SlotStatus::Running,
            output: Some("old".into()),
            details: Some(JsonValue::Null),
            ..SlotProgress::new(format!("c{id}"), "tool")
        },
        parent_call_id: format!("c{parent}").into(),
        parent_task_id: TaskId::from_raw(parent),
        task_id: TaskId::from_raw(id),
        arguments: JsonObject::default().into_value(),
        summary: None,
    };
    let mut live = LiveState {
        nested_tools: Some(vec![nested(2, 1), nested(3, 2), nested(4, 3), nested(5, 1)]),
        ..Default::default()
    };
    finish_slot(
        tool_slot(&mut live, TaskId::from_raw(2))
            .unwrap()
            .progress(),
    );
    let root = &live.nested_tools.as_ref().unwrap()[0];
    assert_eq!(root.progress.status, SlotStatus::Done);
    assert!(root.progress.output.is_none() && root.progress.details.is_none());
    remove_nested_slots(&mut live, TaskId::from_raw(2));
    assert_eq!(
        live.nested_tools
            .as_ref()
            .unwrap()
            .iter()
            .map(|slot| slot.task_id.get())
            .collect::<Vec<_>>(),
        [2, 5]
    );
    for id in [10, 11] {
        add_compaction_status(
            &mut live,
            CompactionStatus {
                task_id: TaskId::from_raw(id),
                reason: CompactionReason::Overflow,
                blocking: false,
                attempt: 1,
                retry: None,
            },
        );
    }
    fail_compaction_status(&mut live, TaskId::from_raw(10), "failed".into());
    assert_eq!(
        live.compaction_failure.as_ref().unwrap().task_id,
        TaskId::from_raw(10)
    );
    remove_compaction_status(&mut live, TaskId::from_raw(11));
    assert!(live.compactions.is_none());
}

#[tokio::test]
async fn admission_storage_failures_poison_later_operations_and_preserve_error_identity() {
    for stage in 0..3 {
        let opened = open().await;
        let failure = Error::message("disk gone");
        *opened.storage.fault.lock() = Some(match stage {
            0 => support::Fault::Before(failure.clone()),
            1 => support::Fault::After(failure.clone()),
            _ => support::Fault::SkipAdopt,
        });
        let rejected = opened
            .service
            .submit(
                opened.root,
                input("not acknowledged", None, None),
                &Context::default(),
            )
            .await
            .err()
            .unwrap();
        if stage < 2 {
            assert!(rejected.same_error(&failure));
        }
        let later = opened
            .service
            .get(SubmissionId::from_raw(999), &Context::default())
            .await
            .err()
            .unwrap();
        assert!(
            matches!(later.kind(),ErrorKind::SessionFailed {cause} if stage==2||cause.same_error(&failure))
        );
        assert!(matches!(
            opened.session.closed().await,
            SessionEnd::Failed(_)
        ));
        opened.session.close(&Context::default()).await.unwrap();
    }
}

#[tokio::test]
async fn submission_traces_link_inputs_to_callers_and_runs_without_recording_payloads() {
    use pi_telemetry::{
        InMemoryTelemetryContext, SpanOptions, SpanOutcome, SpanStatus, TelemetryContext,
    };
    let opened = open().await;
    let telemetry = InMemoryTelemetryContext::new();
    let caller = telemetry.begin_span(SpanOptions::new("caller"));
    let context = pi_chord::context::with_telemetry(caller.clone(), &Context::default());
    let service = Submissions::new(
        opened.session.clone(),
        SubmissionsOptions {
            now: Arc::new(|| 42.0),
            queue_modes: Arc::new(|| Ok(QueueModes::default())),
            resume: Arc::new(|| Ok(())),
            telemetry: telemetry.clone(),
        },
    )
    .unwrap();
    let first = service
        .submit(opened.root, input("private content", None, None), &context)
        .await
        .unwrap();
    service
        .submit(
            opened.root,
            SubmissionDraft::write(EntryDraft::new("note")),
            &context,
        )
        .await
        .unwrap();
    service
        .submit(
            opened.root,
            input("private steer", Some(WhenBusy::Steer), None),
            &context,
        )
        .await
        .unwrap();
    let invalid = SubmissionDraft::Input {
        request_id: None,
        content: "private invalid".into(),
        when_busy: None,
        entry: Some(InputEntry::new("pi.private-kind")),
    };
    assert!(
        service
            .submit(opened.root, invalid, &context)
            .await
            .is_err()
    );
    caller.end(SpanOutcome::Ok);
    let spans = telemetry.get_spans();
    assert_eq!(spans.len(), 5);
    let input = &spans[1];
    let write = &spans[2];
    let steer = &spans[3];
    let failed = &spans[4];
    assert!(input.parent_id.is_none());
    assert_eq!(input.links.len(), 1);
    assert_eq!(input.links[0].context, spans[0].context);
    assert_eq!(write.parent_id, Some(spans[0].id));
    assert!(steer.parent_id.is_none());
    assert_eq!(steer.links.len(), 2);
    assert_eq!(steer.links[1].context, input.context);
    assert_eq!(failed.status, SpanStatus::Error(None));
    assert!(spans.iter().all(|span| span.settled));
    assert!(!serde_json::to_string(&spans).unwrap().contains("private"));
    let record = first.status(&Context::default()).await.unwrap();
    assert_eq!(
        record.info.trace,
        Some(
            JsonObject::try_from_value(pi_chord::json::to_value(&input.context).unwrap()).unwrap()
        )
    );
    opened.session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn an_admitted_wait_retains_its_observer_after_the_caller_drops_the_controller() {
    let Opened {
        session,
        service,
        root,
        ..
    } = open().await;
    let handle = service
        .submit(root, input("active", None, None), &Context::default())
        .await
        .unwrap();
    let id = handle.id;
    let waiting = service.wait(id, &Context::default());
    drop(handle);
    drop(service);
    session
        .commit(
            move |tx| async move {
                let answer = tx.append_entry(root, EntryDraft::new("answer")).await?;
                tx.settle_submission(id, SubmissionSettlement::Done { answer: answer.id })
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let receipt = tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(receipt.state, SubmissionState::InputDone { .. }));
    session.close(&Context::default()).await.unwrap();
}
