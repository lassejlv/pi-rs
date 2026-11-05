use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_durable::{
    harness::{agent::*, catalog::TaskCatalog, reader::*, registry::*, scheduler::*, settings::*},
    *,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct Tool {
    name: JsonString,
    callers: Option<Vec<ToolCaller>>,
    deferred: bool,
}
impl RegisteredTool for Tool {
    fn name(&self) -> &JsonString {
        &self.name
    }
    fn callers(&self) -> Option<&[ToolCaller]> {
        self.callers.as_deref()
    }
    fn deferred(&self) -> bool {
        self.deferred
    }
}
struct Section(JsonString);
impl RegisteredSection for Section {
    fn key(&self) -> &JsonString {
        &self.0
    }
}
type Ext = Extension<Tool, Section>;
fn tool(name: &str) -> Arc<Tool> {
    Arc::new(define_tool(Tool {
        name: name.into(),
        ..Default::default()
    }))
}
fn json(value: &str) -> JsonValue {
    pi_chord::json::parse(value).unwrap()
}
fn state(value: &str) -> AgentState {
    serde_json::from_str(value).unwrap()
}
fn names(tools: &[Arc<Tool>]) -> Vec<String> {
    tools.iter().map(|tool| tool.name.to_string()).collect()
}
fn task(name: &str, version: u64) -> AnyTask {
    let run: PhaseHandler<JsonValue, JsonValue, JsonValue> = Arc::new(|_, runtime, context| {
        Box::pin(async move {
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
        })
    });
    define_task(TaskDefinition {
        name: name.into(),
        version,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"run"}"#))),
        phases: IndexMap::from([("run".into(), run.clone())]),
        abort: run,
        migrate: None,
        hooks: None,
    })
    .erased()
}

#[test]
fn publication_is_atomic_preserves_old_snapshots_and_validates_the_replacement() {
    let builtin = task("pi.generation", 1);
    let registry = Registry::<Tool, Section>::new([builtin.clone()]).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let out = events.clone();
    let read = registry.clone();
    let subscription = registry.subscribe(move || {
        out.lock().push(
            read.snapshot()
                .installed()
                .iter()
                .map(|e| e.name.clone())
                .collect::<Vec<_>>(),
        )
    });
    let mut a = Ext::new("a");
    a.tools.push(tool("read"));
    a.tasks.push(task("app.index", 1));
    let a = Arc::new(a);
    registry.install(a.clone()).unwrap();
    registry.install(Arc::new(Ext::new("b"))).unwrap();
    let before = registry.snapshot();
    let mut invalid = Ext::new("x");
    invalid.tasks.push(task("app.index", 2));
    assert!(
        registry
            .install(Arc::new(invalid))
            .unwrap_err()
            .to_string()
            .contains("already installed")
    );
    assert!(registry.snapshot().same_snapshot(&before));
    assert_eq!(events.lock().len(), 2);
    let mut next = Ext::new("a");
    next.tasks.push(task("app.index", 2));
    registry.install(Arc::new(next)).unwrap();
    assert!(Arc::ptr_eq(&before.extension(&"a".into()).unwrap(), &a));
    assert_eq!(before.task(&"app.index".into()).unwrap().version(), 1);
    assert_eq!(
        registry
            .snapshot()
            .task(&"app.index".into())
            .unwrap()
            .version(),
        2
    );
    assert!(registry.snapshot().tasks()[0].same_definition(&builtin));
    registry.uninstall(&"a".into()).unwrap();
    registry.uninstall(&"a".into()).unwrap();
    registry.install(a).unwrap();
    assert_eq!(
        events.lock().last().unwrap(),
        &[JsonString::from("b"), "a".into()]
    );
    drop(subscription);
    registry.uninstall(&"a".into()).unwrap();
    assert_eq!(events.lock().len(), 5);
}

#[test]
fn invalid_names_and_duplicate_targets_do_not_publish() {
    let registry = Registry::<Tool, Section>::new([]).unwrap();
    let before = registry.snapshot();
    for keys in [
        vec!["a", "a"],
        vec!["Bad Key"],
        vec![""],
        vec!["instructions"],
        vec!["é"],
    ] {
        let mut ext = Ext::new("bad");
        ext.sections = keys
            .into_iter()
            .map(|key| Arc::new(Section(key.into())))
            .collect();
        assert!(registry.install(Arc::new(ext)).is_err());
        assert!(registry.snapshot().same_snapshot(&before));
    }
    let mut ext = Ext::new("bad");
    ext.tools = vec![tool("same"), tool("same")];
    assert!(registry.install(Arc::new(ext)).is_err());
    let mut valid = Ext::new("valid");
    valid.sections.push(Arc::new(Section("a-0_".into())));
    registry.install(Arc::new(valid)).unwrap();
}

#[test]
fn listeners_are_snapshotted_and_a_failure_retains_the_publication() {
    let registry = Registry::<Tool, Section>::new([]).unwrap();
    let later = Arc::new(Mutex::new(None));
    let remove = later.clone();
    let _first = registry.subscribe_fallible(move || {
        remove.lock().take();
        Err(Error::message("listener failed"))
    });
    let calls = Arc::new(Mutex::new(0));
    let out = calls.clone();
    *later.lock() = Some(registry.subscribe(move || *out.lock() += 1));
    assert_eq!(
        registry
            .install(Arc::new(Ext::new("published")))
            .unwrap_err()
            .to_string(),
        "listener failed"
    );
    assert!(registry.snapshot().extension(&"published".into()).is_some());
    assert_eq!(*calls.lock(), 0);
}

#[test]
fn unsubscribing_during_delivery_takes_effect_at_the_next_publication() {
    let registry = Registry::<Tool, Section>::new([]).unwrap();
    let later = Arc::new(Mutex::new(None));
    let remove = later.clone();
    let _first = registry.subscribe(move || {
        remove.lock().take();
    });
    let calls = Arc::new(Mutex::new(0));
    let out = calls.clone();
    *later.lock() = Some(registry.subscribe(move || *out.lock() += 1));
    registry.install(Arc::new(Ext::new("one"))).unwrap();
    registry.install(Arc::new(Ext::new("two"))).unwrap();
    assert_eq!(*calls.lock(), 1);
}

#[test]
fn a_failing_reporter_stops_later_wrappers() {
    let registry = Registry::<Tool, Section>::new([]).unwrap();
    let mut ext = Ext::new("broken");
    ext.tools = vec![tool("one"), tool("two")];
    ext.wraps.push(Wrap::Tool {
        name: "one".into(),
        apply: Arc::new(|_| Err(Error::message("wrapper"))),
    });
    ext.wraps.push(Wrap::Tool {
        name: "two".into(),
        apply: Arc::new(|_| panic!("reporting already failed")),
    });
    registry.install(Arc::new(ext)).unwrap();
    let result = try_resolve_agent(
        None,
        &registry.snapshot(),
        &resolve_settings(HarnessSettings::default()),
        |_, _| Err(Error::message("report failed")),
    );
    assert_eq!(result.err().unwrap().to_string(), "report failed");
}

#[test]
fn agent_composition_filters_callers_and_loads_deferred_tools_in_requested_order() {
    let registry = Registry::<Tool, Section>::new([]).unwrap();
    let mut ext = Ext::new("coding");
    ext.tools = vec![
        tool("read"),
        Arc::new(Tool {
            name: "internal".into(),
            callers: Some(vec![ToolCaller::Tools]),
            deferred: false,
        }),
        Arc::new(Tool {
            name: "search".into(),
            deferred: true,
            ..Default::default()
        }),
        Arc::new(Tool {
            name: "browse".into(),
            deferred: true,
            ..Default::default()
        }),
    ];
    registry.install(Arc::new(ext)).unwrap();
    let settings = resolve_settings(HarnessSettings::default());
    let a = resolve_agent(
        Some(&state(
            r#"{"modelTools":{"add":["browse","search","browse"],"remove":["read"]}}"#,
        )),
        &registry.snapshot(),
        &settings,
        |_, _| panic!("unexpected report"),
    )
    .unwrap();
    assert_eq!(names(&a.tools), ["browse", "search"]);
    assert_eq!(names(&a.callable), ["read", "internal", "search", "browse"]);
    assert!(a.deferred.is_empty());
    let a = resolve_agent(
        Some(&state(
            r#"{"modelTools":["search","internal","read","search"]}"#,
        )),
        &registry.snapshot(),
        &settings,
        |_, _| {},
    )
    .unwrap();
    assert_eq!(names(&a.tools), ["search", "read"]);
    assert_eq!(names(&a.deferred), ["browse"]);
}

#[test]
fn wrappers_run_before_filters_drop_failures_and_never_wrap_instructions() {
    let registry = Registry::<Tool, Section>::new([]).unwrap();
    let mut ext = Ext::new("tools");
    ext.tools = vec![tool("read"), tool("edit")];
    ext.sections.push(Arc::new(Section("cwd".into())));
    registry.install(Arc::new(ext)).unwrap();
    let mut ext = define_extension(Ext::new("wrap"));
    ext.wraps.push(wrap_tool(tool("read").as_ref(), |_| {
        Err(Error::message("broken"))
    }));
    ext.wraps
        .push(wrap_tool(tool("edit").as_ref(), |_| Ok(tool("renamed"))));
    ext.wraps
        .push(wrap_section("instructions", |_| panic!("must not run")));
    registry.install(Arc::new(ext)).unwrap();
    let mut errors = Vec::new();
    let a = resolve_agent(
        Some(&state(r#"{"tools":[],"instructions":""}"#)),
        &registry.snapshot(),
        &resolve_settings(HarnessSettings::default()),
        |error, ext| errors.push((error.to_string(), ext.clone())),
    )
    .unwrap();
    assert_eq!(
        errors,
        [
            ("broken".into(), "wrap".into()),
            ("Wrapper renamed edit to renamed".into(), "wrap".into())
        ]
    );
    assert!(a.tools.is_empty());
    assert_eq!(
        a.sections
            .iter()
            .map(|s| s.key().to_string())
            .collect::<Vec<_>>(),
        ["cwd", "instructions"]
    );
    assert_eq!(
        section_extension(&a, &"instructions".into()),
        Some("wrap".into())
    );
    assert_eq!(tool_extension(&a, &"read".into()), Some("wrap".into()));
}

#[test]
fn settings_defaults_merge_without_losing_unknown_policy_fields_or_nonfinite_runtime_values() {
    let input: HarnessSettings = serde_json::from_str(r#"{"retry":{"enabled":false,"custom":"\ud800"},"progress":{"partialIntervalMs":null,"outputIntervalMs":250},"restartLimit":2.9}"#).unwrap();
    let resolved = resolve_settings(input);
    assert_eq!(resolved.retry[&JsonString::from("maxRetries")], json("3"));
    assert_eq!(resolved.retry[&JsonString::from("enabled")], json("false"));
    assert_eq!(
        resolved.retry[&JsonString::from("custom")],
        json(r#""\ud800""#)
    );
    assert_eq!(resolved.progress.partial_interval_ms, 100.0);
    assert_eq!(resolved.progress.output_interval_ms, 250.0);
    assert_eq!(resolved.restart_limit, 2.0);
    let resolved = resolve_settings(HarnessSettings {
        context_retention_ms: Some(f64::NAN),
        document_retention_ms: Some(-1.0),
        restart_limit: Some(f64::INFINITY),
        ..Default::default()
    });
    assert!(resolved.context_retention_ms.is_nan());
    assert_eq!(resolved.document_retention_ms, 600_000.0);
    assert_eq!(resolved.restart_limit, f64::INFINITY);
}

#[tokio::test]
async fn configuration_is_rewindable_and_add_tools_preserves_surrogate_names() {
    let session = Session::new(Arc::new(MemoryStorage::new()), SessionOptions::default());
    let context = Context::default();
    let (parent, at) = session.commit(|tx| async move {
        let parent = tx.create_root_conversation().await?; create_agent(&tx, parent.clone()).await?;
        tx.doc(&AGENT_DOC,parent.id).await?.update(|state| { state.0.insert("custom".into(), json(r#""\udc00""#)); Ok(()) })?;
        configure(&tx, parent.id, state(r#"{"instructions":"old","tools":{"remove":[{"name":"\ud800"},{"name":"other"}]}}"#).0).await?;
        let at = tx.append_entry(parent.id, EntryDraft::new("point")).await?.id;
        Ok((parent.id,at))
    }, &context).await.unwrap();
    session
        .commit(
            move |tx| async move {
                configure(&tx, parent, state(r#"{"instructions":"new","cwd":null}"#).0).await?;
                add_tools(
                    &tx,
                    parent,
                    serde_json::from_str(r#"["\ud800","\ud800"]"#).unwrap(),
                )
                .await
            },
            &context,
        )
        .await
        .unwrap();
    let latest = session
        .snapshot(&AGENT_DOC, parent, &context)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        latest.0[&JsonString::from("tools")],
        json(r#"{"remove":["other"]}"#)
    );
    assert_eq!(
        latest.0[&JsonString::from("modelTools")],
        json(r#"{"add":["\ud800"]}"#)
    );
    assert_eq!(latest.0[&JsonString::from("custom")], json(r#""\udc00""#));
    let fork = session
        .commit(
            move |tx| async move {
                let fork = tx
                    .fork_conversation(parent, at, ConversationOwnership::Ownerless)
                    .await?;
                create_agent(&tx, fork.clone()).await?;
                Ok(fork.id)
            },
            &context,
        )
        .await
        .unwrap();
    let forked = session
        .snapshot(&AGENT_DOC, fork, &context)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        forked.0[&JsonString::from("instructions")],
        json(r#""old""#)
    );
    assert!(!forked.0.contains_key(&JsonString::from("modelTools")));
    session.close(&context).await.unwrap();
}

#[tokio::test]
async fn an_owned_conversation_copies_its_owners_stored_agent_independently() {
    let session = Session::new(Arc::new(MemoryStorage::new()), SessionOptions::default());
    let context = Context::default();
    let (parent, child) = session
        .commit(
            |tx| async move {
                let parent = tx.create_root_conversation().await?;
                create_agent(&tx, parent.clone()).await?;
                configure(
                    &tx,
                    parent.id,
                    state(r#"{"instructions":"inherited","extensions":[{"name":"coding"}]}"#).0,
                )
                .await?;
                let task = tx
                    .create_task_record(
                        "owner".into(),
                        1,
                        JsonValue::Null,
                        |_| Ok(json(r#"{"phase":"run"}"#)),
                        TaskOptions {
                            conversation_id: Some(parent.id),
                            ..TaskOptions::new(TaskOwnership::Conversation)
                        },
                    )
                    .await?;
                let child = tx
                    .create_conversation(ConversationOwnership::Task(task))
                    .await?;
                create_agent(&tx, child.clone()).await?;
                Ok((parent.id, child.id))
            },
            &context,
        )
        .await
        .unwrap();
    session
        .commit(
            move |tx| async move {
                configure(
                    &tx,
                    parent,
                    state(r#"{"instructions":null,"cwd":"/parent"}"#).0,
                )
                .await
            },
            &context,
        )
        .await
        .unwrap();
    let child = session
        .snapshot(&AGENT_DOC, child, &context)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        child.0[&JsonString::from("instructions")],
        json(r#""inherited""#)
    );
    assert_eq!(
        child.0[&JsonString::from("extensions")],
        json(r#"["coding"]"#)
    );
    assert!(!child.0.contains_key(&JsonString::from("cwd")));
    session.close(&context).await.unwrap();
}

#[tokio::test]
async fn the_runtime_keeps_its_full_registry_until_the_next_durable_phase() {
    let registry = Registry::<Tool, Section>::new([]).unwrap();
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let held = Arc::new(Mutex::new(None));
    let first = Arc::new(Mutex::new(None::<RegistryView>));
    let handler: PhaseHandler<JsonValue, JsonValue, JsonValue> = {
        let (entered, gate, seen, held, first) = (
            entered.clone(),
            gate.clone(),
            seen.clone(),
            held.clone(),
            first.clone(),
        );
        Arc::new(move |task, runtime, context| {
            let (entered, gate, seen, held, first) = (
                entered.clone(),
                gate.clone(),
                seen.clone(),
                held.clone(),
                first.clone(),
            );
            Box::pin(async move {
                let phase = task.checkpoint()["phase"].to_utf8().unwrap();
                let before = runtime.registry();
                if phase == "a" {
                    *first.lock() = Some(before.clone());
                    *held.lock() = Some(runtime.clone());
                    entered.add_permits(1);
                    gate.acquire().await.unwrap().forget();
                }
                let view = runtime.registry();
                let full = view.typed::<Tool, Section>()?;
                seen.lock().push((
                    phase.clone(),
                    full.tools()
                        .iter()
                        .map(|(_, t)| t.name.to_string())
                        .collect::<Vec<_>>(),
                    view.same_snapshot(&before),
                ));
                if phase == "a" {
                    runtime
                        .commit(
                            |_, _| async {
                                Ok(Some(NextTaskState::Running {
                                    checkpoint: json(r#"{"phase":"b"}"#),
                                }))
                            },
                            &context,
                        )
                        .await
                } else {
                    assert!(!view.same_snapshot(first.lock().as_ref().unwrap()));
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
                }
            })
        })
    };
    let definition = define_task(TaskDefinition {
        name: "test.snapshots".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"a"}"#))),
        phases: IndexMap::from([("a".into(), handler.clone()), ("b".into(), handler.clone())]),
        abort: handler,
        migrate: None,
        hooks: None,
    });
    let mut tasks = Ext::new("tasks");
    tasks.tasks.push(definition.erased());
    registry.install(Arc::new(tasks)).unwrap();
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        registry.clone(),
        SchedulerOptions::default(),
        SessionOptions::default(),
        &Context::default(),
    )
    .await
    .unwrap();
    let id = scheduler
        .session()
        .commit(
            move |tx| async move {
                let root = tx.create_root_conversation().await?;
                tx.create_task(
                    &definition,
                    JsonValue::Null,
                    TaskOptions {
                        conversation_id: Some(root.id),
                        ..TaskOptions::new(TaskOwnership::Conversation)
                    },
                )
                .await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    scheduler.resume().unwrap();
    entered.acquire().await.unwrap().forget();
    let mut late = Ext::new("late");
    late.tools.push(tool("late"));
    registry.install(Arc::new(late)).unwrap();
    assert!(
        held.lock()
            .as_ref()
            .unwrap()
            .registry()
            .same_snapshot(first.lock().as_ref().unwrap())
    );
    gate.add_permits(1);
    scheduler
        .wait_for_task(id.untyped(), &Context::default())
        .await
        .unwrap();
    assert_eq!(
        *seen.lock(),
        [
            ("a".into(), vec![], true),
            ("b".into(), vec!["late".into()], true)
        ]
    );
    assert_eq!(
        held.lock()
            .as_ref()
            .unwrap()
            .registry()
            .typed::<Tool, Section>()
            .unwrap()
            .tools()[0]
            .1
            .name,
        JsonString::from("late")
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[derive(Clone, Default)]
struct HostReader {
    catalog: TaskCatalog,
    reads: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
    unsubscribed: Arc<AtomicUsize>,
    fail_subscribe: bool,
    fail_unsubscribe: bool,
}
impl RegistryReader for HostReader {
    fn snapshot(&self) -> Result<RegistryView> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if self.fail.load(Ordering::Relaxed) {
            Err(Error::message("registry failed"))
        } else {
            RegistryReader::snapshot(&self.catalog)
        }
    }
    fn subscribe(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Result<ReaderSubscription> {
        if self.fail_subscribe {
            return Err(Error::message("subscription failed"));
        }
        let subscription = RegistryReader::subscribe(&self.catalog, listener)?;
        let count = self.unsubscribed.clone();
        let fail = self.fail_unsubscribe;
        Ok(ReaderSubscription::new(move || {
            subscription.close()?;
            count.fetch_add(1, Ordering::Relaxed);
            if fail {
                Err(Error::message("unsubscribe failed"))
            } else {
                Ok(())
            }
        }))
    }
}

#[tokio::test]
async fn an_idle_scheduler_reads_no_snapshot_and_blocked_lookup_reports_a_failure_once() {
    let reader = HostReader::default();
    reader.fail.store(true, Ordering::Relaxed);
    let reports = Arc::new(Mutex::new(Vec::new()));
    let out = reports.clone();
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        reader.clone(),
        SchedulerOptions {
            on_report: Arc::new(move |error, info, _| {
                out.lock().push((error.to_string(), info.callback))
            }),
            ..Default::default()
        },
        SessionOptions::default(),
        &Context::default(),
    )
    .await
    .unwrap();
    // Open schedules a recovery cascade even for an empty session. Its work
    // counts until the pass settles, just as in the reference.
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while scheduler.wake().active {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!scheduler.wake().active);
    assert_eq!(reader.reads.load(Ordering::Relaxed), 0);
    let record:TaskRecord=serde_json::from_str(r#"{"id":99,"conversationId":1,"kind":"unknown","version":1,"input":null,"background":false,"abortRequested":false,"state":{"status":"pending","checkpoint":{"phase":"run"}}}"#).unwrap();
    assert!(scheduler.blocked_reason(&record).is_none());
    assert!(scheduler.blocked_reason(&record).is_none());
    assert_eq!(
        *reports.lock(),
        [("registry failed".into(), Some("registry"))]
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
    assert_eq!(reader.unsubscribed.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn a_failed_registry_subscription_closes_the_opened_storage() {
    let storage = Arc::new(MemoryStorage::new());
    let reader = HostReader {
        fail_subscribe: true,
        ..Default::default()
    };
    let error = TaskScheduler::open(
        storage.clone(),
        reader,
        SchedulerOptions::default(),
        SessionOptions::default(),
        &Context::default(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.to_string(), "subscription failed");
    assert!(storage.mint_id().await.is_err());
}

#[tokio::test]
async fn an_unsubscribe_failure_is_reported_and_does_not_interrupt_close() {
    let reader = HostReader {
        fail_unsubscribe: true,
        ..Default::default()
    };
    let reports = Arc::new(Mutex::new(Vec::new()));
    let out = reports.clone();
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        reader.clone(),
        SchedulerOptions {
            on_report: Arc::new(move |error, info, _| {
                out.lock().push((error.to_string(), info.callback))
            }),
            ..Default::default()
        },
        SessionOptions::default(),
        &Context::default(),
    )
    .await
    .unwrap();
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
    assert_eq!(
        *reports.lock(),
        [("unsubscribe failed".into(), Some("registry"))]
    );
    assert_eq!(reader.unsubscribed.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn wake_is_conservative_on_snapshot_failure_and_a_failed_reservation_fails_the_session() {
    let reader = HostReader::default();
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        reader.clone(),
        SchedulerOptions::default(),
        SessionOptions::default(),
        &Context::default(),
    )
    .await
    .unwrap();
    let id = scheduler
        .session()
        .commit(
            |tx| async move {
                let root = tx.create_root_conversation().await?;
                tx.create_task_record(
                    "missing".into(),
                    1,
                    JsonValue::Null,
                    |_| Ok(json(r#"{"phase":"run"}"#)),
                    TaskOptions {
                        conversation_id: Some(root.id),
                        ..TaskOptions::new(TaskOwnership::Conversation)
                    },
                )
                .await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while scheduler.wake().active {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    reader.fail.store(true, Ordering::Relaxed);
    assert!(scheduler.wake().active);
    scheduler.resume().unwrap();
    assert!(
        scheduler
            .wait_for_task(id, &Context::default())
            .await
            .is_err()
    );
    assert_eq!(
        scheduler.session().failure().unwrap().to_string(),
        "registry failed"
    );
    let _ = scheduler.session().close(&Context::default()).await;
}
