//! Direct tests of the harness internals the stock suites reach only through whole runs:
//! `safe_now`, `admit_submission`, `end_run`, `convert_partial` and `ensure_provider_session_id`.
//! Expected behaviour is read from upstream `harness/harness.ts`, `submissions.ts`, `live.ts`,
//! `generation.ts` and `provider.ts`.
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_durable::{
    harness::{
        catalog::*,
        live::{LIVE_DOC, LiveGeneration, LiveRun, convert_partial, end_run},
        provider::{PROVIDER_DOC, ensure_provider_session_id},
        runtime::TaskRuntime,
        scheduler::*,
        state::NextTaskState,
        submissions::{AdmissionStatus, admit_submission},
        types::{QueueMode, QueueModes},
        usage::USAGE_DOC,
        {HarnessReport, safe_now},
    },
    *,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
fn modes() -> QueueModes {
    QueueModes {
        steering_mode: QueueMode::OneAtATime,
        follow_up_mode: QueueMode::OneAtATime,
    }
}
fn session() -> Session {
    Session::new(Arc::new(MemoryStorage::new()), SessionOptions::default())
}
async fn root(session: &Session) -> ConversationId {
    session
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &Context::default(),
        )
        .await
        .unwrap()
}

#[test]
fn safe_now_falls_back_to_the_wall_clock_and_reports_the_first_failure_only() {
    let reports = Arc::new(AtomicUsize::new(0));
    let counted = reports.clone();
    let report: HarnessReport = Arc::new(move |_, info| {
        assert_eq!((info.kind, info.callback), ("host_error", Some("now")));
        counted.fetch_add(1, Ordering::SeqCst);
    });
    // No host clock: the wall clock, never reported.
    let wall = safe_now(None, report.clone())();
    assert!(wall > 1_600_000_000_000.0);
    // A working host clock is used as it is.
    let steady = safe_now(Some(Arc::new(|| Ok(1234.0))), report.clone());
    assert_eq!((steady(), steady()), (1234.0, 1234.0));
    assert_eq!(reports.load(Ordering::SeqCst), 0);
    // Errors and panics fall back to the wall clock; only the first is reported.
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let broken = safe_now(
        Some(Arc::new(move || {
            if seen.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
                Err(Error::message("clock failed"))
            } else {
                panic!("clock panicked")
            }
        })),
        report,
    );
    for _ in 0..4 {
        assert!(broken() > 1_600_000_000_000.0);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(reports.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn admission_deduplicates_by_request_id_and_validates_input_entries() {
    let session = session();
    let context = Context::default();
    let conversation = root(&session).await;
    let admit = |draft: SubmissionDraft| {
        let session = session.clone();
        let context = context.clone();
        async move {
            session
                .commit(
                    move |tx| async move {
                        admit_submission(&tx, conversation, &draft, 10.0, modes(), None).await
                    },
                    &context,
                )
                .await
        }
    };
    let input = |request: &str| SubmissionDraft::Input {
        request_id: Some(request.into()),
        content: "hi".into(),
        when_busy: None,
        entry: None,
    };
    let first = admit(input("r1")).await.unwrap();
    // An input to an idle conversation is placed at once.
    assert_eq!(first.status, AdmissionStatus::Placed);
    // The same request ID finds the submission admitted before.
    let again = admit(input("r1")).await.unwrap();
    assert_eq!(
        (again.id, again.status),
        (first.id, AdmissionStatus::Placed)
    );
    assert_ne!(admit(input("r2")).await.unwrap().id, first.id);
    // A request ID names one kind of submission.
    let write = SubmissionDraft::Write {
        request_id: Some("r1".into()),
        entry: EntryDraft::new("note"),
    };
    let error = admit(write).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("already identifies a submission of type input"),
        "{error}"
    );
    // An input's own entry kind must be non-empty and outside the `pi.` namespace.
    for kind in ["", "pi.user"] {
        let draft = SubmissionDraft::Input {
            request_id: None,
            content: "x".into(),
            when_busy: None,
            entry: Some(InputEntry::new(kind)),
        };
        let error = admit(draft).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("must be a non-empty string not starting with"),
            "{kind:?}: {error}"
        );
    }
    // A display-only write needs no boundary: it settles at once.
    let note = admit(SubmissionDraft::Write {
        request_id: None,
        entry: EntryDraft::new("note"),
    })
    .await
    .unwrap();
    assert_eq!(note.status, AdmissionStatus::Done);
}

#[tokio::test]
async fn ending_a_run_settles_its_inputs_and_clears_the_live_run() {
    let session = session();
    let context = Context::default();
    let conversation = root(&session).await;
    let id = session
        .commit(
            move |tx| async move {
                let draft = SubmissionDraft::Input {
                    request_id: None,
                    content: "go".into(),
                    when_busy: None,
                    entry: None,
                };
                Ok(
                    admit_submission(&tx, conversation, &draft, 1.0, modes(), None)
                        .await?
                        .id,
                )
            },
            &context,
        )
        .await
        .unwrap();
    session
        .commit(
            move |tx| async move {
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                live.update(|state| {
                    state.run = Some(LiveRun {
                        task_id: TaskId::from_raw(50),
                        inputs: vec![id],
                    });
                    state.generation = Some(LiveGeneration {
                        attempt: 1,
                        message: None,
                        retry: None,
                        deferred: None,
                    });
                    state.tools = Some(Vec::new());
                    Ok(())
                })
            },
            &context,
        )
        .await
        .unwrap();
    let settle = |task: i64| {
        let session = session.clone();
        let context = context.clone();
        async move {
            session
                .commit(
                    move |tx| async move {
                        let live = tx.doc(&LIVE_DOC, conversation).await?;
                        end_run(
                            &tx,
                            &live,
                            TaskId::from_raw(task),
                            SubmissionSettlement::Unanswered {
                                reason: "run_failed".into(),
                                detail: None,
                            },
                        )?;
                        live.read()
                    },
                    &context,
                )
                .await
        }
    };
    // Another task's end leaves this run's inputs alone, but still clears the generation and tools.
    let other = settle(51).await.unwrap();
    assert!(other.run.is_some() && other.generation.is_none() && other.tools.is_none());
    let submission = session
        .commit(move |tx| async move { tx.submission(id).await }, &context)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(submission.state, SubmissionState::InputPlaced { .. }),
        "{:?}",
        submission.state
    );
    // The run's own end settles each input and removes the run.
    let own = settle(50).await.unwrap();
    assert!(own.run.is_none());
    let submission = session
        .commit(move |tx| async move { tx.submission(id).await }, &context)
        .await
        .unwrap()
        .unwrap();
    match submission.state {
        SubmissionState::InputUnanswered { reason, .. } => {
            assert_eq!(reason.to_string_lossy(), "run_failed")
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_partial_answer_becomes_an_aborted_assistant_entry_with_its_usage() {
    let session = session();
    let context = Context::default();
    let conversation = root(&session).await;
    let message = json(
        r#"{"role":"assistant","content":[{"type":"text","text":"half"}],"api":"faux","provider":"faux","model":"faux-1","stopReason":"stop","timestamp":5,"usage":{"input":3,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":5,"cost":{"input":0.5,"output":0.25,"cacheRead":0,"cacheWrite":0,"total":0.75}}}"#,
    );
    let message = JsonObject::try_from_value(message).unwrap();
    session
        .commit(
            move |tx| async move {
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                live.update(|state| {
                    state.generation = Some(LiveGeneration {
                        attempt: 1,
                        message: Some(message),
                        retry: None,
                        deferred: None,
                    });
                    Ok(())
                })?;
                convert_partial(&tx, &live, conversation).await
            },
            &context,
        )
        .await
        .unwrap();
    let (entry, usage) = session
        .commit(
            move |tx| async move {
                let query = EntryQuery {
                    conversation_id: conversation,
                    min_entry_id: None,
                    max_entry_id: None,
                    order: None,
                };
                let entries = tx.scan_entries(query, 100, None).await?.items;
                let usage = tx.doc(&USAGE_DOC, conversation).await?.read()?;
                Ok((entries, usage))
            },
            &context,
        )
        .await
        .unwrap();
    let assistant = entry
        .iter()
        .find(|entry| entry.kind == *ASSISTANT_ENTRY.kind())
        .expect("an assistant entry");
    let stored = serde_json::to_value(&assistant.model.as_ref().unwrap()[0]).unwrap();
    assert_eq!(stored["stopReason"], "aborted");
    assert_eq!(stored["content"][0]["text"], "half");
    assert_eq!(
        usage.models[&JsonString::from("faux/faux-1")].value()["input"],
        json("3")
    );
    // Without a partial message nothing is written.
    session
        .commit(
            move |tx| async move {
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                live.update(|state| {
                    state.generation = None;
                    Ok(())
                })?;
                convert_partial(&tx, &live, conversation).await
            },
            &context,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn the_provider_session_id_is_created_once_and_then_read() {
    let ids = Arc::new(Mutex::new(Vec::new()));
    let log = ids.clone();
    let task: Task<JsonValue, JsonValue, JsonValue> = define_task(TaskDefinition {
        name: "provider-id".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"run"}"#))),
        phases: IndexMap::from([(
            "run".into(),
            Arc::new(move |_, runtime: TaskRuntime, context: Context| {
                let log = log.clone();
                Box::pin(async move {
                    // A conversation without `pi.provider` gets one migration commit.
                    assert!(
                        runtime
                            .snapshot(&PROVIDER_DOC, runtime.conversation_id(), &context)
                            .await?
                            .is_none()
                    );
                    let first = ensure_provider_session_id(&runtime, &context).await?;
                    let second = ensure_provider_session_id(&runtime, &context).await?;
                    let stored = runtime
                        .snapshot(&PROVIDER_DOC, runtime.conversation_id(), &context)
                        .await?;
                    log.lock()
                        .push((first, second, stored.map(|state| state.session_id.clone())));
                    runtime
                        .commit(
                            |_, _| async {
                                Ok(Some(NextTaskState::Terminal {
                                    outcome: TaskOutcome::Completed {
                                        result: JsonValue::Null,
                                    },
                                }))
                            },
                            &context,
                        )
                        .await
                }) as storage::StorageFuture<()>
            }) as PhaseHandler<JsonValue, JsonValue, JsonValue>,
        )]),
        abort: Arc::new(|_, _, _| Box::pin(async { Ok(()) })),
        migrate: None,
        hooks: None,
    });
    let catalog = TaskCatalog::default();
    catalog
        .replace([InstalledTask {
            task: task.erased(),
            extension: Some("tests".into()),
        }])
        .unwrap();
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        catalog,
        SchedulerOptions::default(),
        SessionOptions::default(),
        &Context::default(),
    )
    .await
    .unwrap();
    let conversation = root(&scheduler.session()).await;
    let started = task.clone();
    let id = scheduler
        .session()
        .commit(
            move |tx| async move {
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(conversation);
                Ok(tx
                    .create_task(&started, JsonValue::Null, options)
                    .await?
                    .untyped())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    scheduler.resume().unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        scheduler.wait_for_task(id, &Context::default()),
    )
    .await
    .expect("task did not settle")
    .unwrap();
    let seen = ids.lock();
    let (first, second, stored) = &seen[0];
    assert_eq!(first, second);
    assert_eq!(stored.as_ref(), Some(first));
    // A UUID.
    assert_eq!(first.to_string_lossy().len(), 36);
}
