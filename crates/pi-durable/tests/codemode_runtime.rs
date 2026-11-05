use pi_ai::{models::Models, providers::faux::*};
use pi_durable::{
    codemode::*,
    harness::{declaration::ToolDeclaration, prompt::NativeSection, registry::Extension, tool::*},
    *,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[tokio::test]
async fn durable_codemode_tool_preserves_raw_utf16_source_through_options_and_storage() {
    use pi_durable::harness::live::{LIVE_DOC, SlotProgress, ToolSlot};
    for retention in [600_000.0, 0.0] {
        let sandbox = Arc::new(vm::NativeSandbox::new().unwrap());
        let registry = create_registry::<NativeTool, NativeSection>().unwrap();
        let mut code = CodemodeToolOptions::new(sandbox.clone());
        code.models = false;
        registry
            .install(Arc::new(create_codemode_extension(code)))
            .unwrap();
        let models = Models::default();
        models.set_provider(faux_provider(Default::default()).provider);
        let mut options = HarnessOptions::new(registry, models);
        options.default_document_retention_ms = retention;
        let context = Context::default();
        let harness = Harness::open(Arc::new(MemoryStorage::default()), options, &context)
            .await
            .unwrap();
        let root = harness
            .root(
                ConversationOptions {
                    agent: Some(
                        JsonObject::try_from_value(
                            pi_chord::json::parse(
                                r#"{"model":{"provider":"faux","modelId":"faux-1"}}"#,
                            )
                            .unwrap(),
                        )
                        .unwrap(),
                    ),
                    ..Default::default()
                },
                &context,
            )
            .await
            .unwrap();
        let conversation = root.id;
        let source: Vec<u16> = "// @options: {\"timeout_ms\":5000}\r\nconst value='"
            .encode_utf16()
            .chain([0xd800])
            .chain("'; store('raw', value); return value;".encode_utf16())
            .collect();
        let task = harness
            .scheduler()
            .session()
            .commit(
                move |tx| async move {
                    let mut arguments = JsonObject::default();
                    arguments.insert(
                        "code".into(),
                        JsonValue::String(JsonString::from_utf16(source)),
                    );
                    let call = ToolCall::new("raw", "codemode", arguments.into_value());
                    let mut message = JsonObject::default();
                    message.insert("role".into(), "assistant".into());
                    message.insert("content".into(), pi_chord::json::to_value(&[call]).unwrap());
                    let mut entry = EntryDraft::new("pi.assistant");
                    entry.model = Some(vec![message.into_value()]);
                    let entry = tx.append_entry(conversation, entry).await?;
                    let mut options = TaskOptions::new(TaskOwnership::Conversation);
                    options.conversation_id = Some(conversation);
                    let id = tx
                        .create_task(&TOOL_TASK, ToolTaskInput::model(entry.id, "raw"), options)
                        .await?;
                    tx.doc(&LIVE_DOC, conversation).await?.update(|live| {
                        live.tools = Some(vec![ToolSlot {
                            progress: SlotProgress::new("raw", "codemode"),
                            task_id: Some(id.untyped()),
                            entry: None,
                        }]);
                        Ok(())
                    })?;
                    Ok(id.untyped())
                },
                &context,
            )
            .await
            .unwrap();
        let record = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            harness.wait_for_task(task, &context),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(
                &record.state,
                TaskState::Terminal {
                    outcome: TaskOutcome::Completed { .. }
                }
            ),
            "{:?}",
            record.state
        );
        let stored = harness
            .snapshot(&CODEMODE_STORE_DOC, conversation, &context)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.values.value()["raw"].as_string().unwrap().units(),
            &[0xd800]
        );
        harness.close(&context).await.unwrap();
        sandbox.0.close().await;
    }
}

#[tokio::test]
async fn native_sqlite_restart_replays_finished_calls_and_resumes_safe_owned_work() {
    use pi_durable::storage::sqlite::open_native_sqlite_storage;
    for retention in [600000.0, 0.0] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("state.sqlite");
        let calls = Arc::new(AtomicUsize::new(0));
        let held = Arc::new(AtomicUsize::new(0));
        let blocked = Arc::new(AtomicBool::new(true));
        let reached = Arc::new(tokio::sync::Notify::new());
        let registry = create_registry::<NativeTool, NativeSection>().unwrap();
        let count = calls.clone();
        let mut extension = Extension::<NativeTool, NativeSection>::new("effects");
        extension.tools.push(Arc::new(NativeTool::new(
            ToolDeclaration::new(
                "echo",
                "Echo once",
                pi_chord::json::parse(r#"{"type":"object","properties":{}}"#).unwrap(),
            ),
            Arc::new(move |_, _, _| {
                count.fetch_add(1, Ordering::Relaxed);
                Box::pin(async {
                    Ok(ToolExecutionResult {
                        output: Some(Arc::new(vec![text_content("echoed".into())])),
                        ..Default::default()
                    })
                })
            }),
        )));
        let count = held.clone();
        let gate = blocked.clone();
        let notice = reached.clone();
        let mut hold = NativeTool::new(
            ToolDeclaration::new(
                "hold",
                "Wait for a restart",
                pi_chord::json::parse(r#"{"type":"object","properties":{}}"#).unwrap(),
            ),
            Arc::new(move |_, _, context| {
                count.fetch_add(1, Ordering::Relaxed);
                let gate = gate.clone();
                let notice = notice.clone();
                Box::pin(async move {
                    if gate.load(Ordering::Acquire) {
                        notice.notify_one();
                        context.abort_signal().unwrap().cancelled().await;
                        context.check()?;
                    }
                    Ok(ToolExecutionResult {
                        output: Some(Arc::new(vec![text_content("resumed".into())])),
                        ..Default::default()
                    })
                })
            }),
        );
        hold.replay = ReplayPolicy::Safe;
        extension.tools.push(Arc::new(hold));
        registry.install(Arc::new(extension)).unwrap();
        let first_sandbox = Arc::new(vm::NativeSandbox::new().unwrap());
        let mut code = CodemodeToolOptions::new(first_sandbox.clone());
        code.models = false;
        registry
            .install(Arc::new(create_codemode_extension(code)))
            .unwrap();
        let faux = faux_provider(RegisterFauxProviderOptions {
            tokens_per_second: Some(f64::INFINITY),
            ..Default::default()
        });
        let checked = Arc::new(AtomicBool::new(false));
        let seen = checked.clone();
        let arguments=serde_json::json!({"code":"const a = await tools.echo({}); const b = await tools.hold({}); store('done', b); return [a, b];"}).as_object().unwrap().clone();
        faux.set_responses(vec![
            faux_assistant_message(
                vec![faux_tool_call("codemode", arguments, Some("replay"))],
                FauxMessageOptions {
                    stop_reason: Some(pi_ai::StopReason::ToolUse),
                    ..Default::default()
                },
            )
            .into(),
            FauxResponseStep::Factory(Arc::new(move |context, _, _, _| {
                let transcript = serde_json::to_value(context).unwrap();
                let receipt = transcript["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|m| m["role"] == "toolResult")
                    .unwrap();
                assert_eq!(receipt["isError"], false, "{receipt}");
                let text = receipt["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v["text"].as_str())
                    .collect::<String>();
                assert!(text.contains(r#"["echoed","resumed"]"#), "{text}");
                seen.store(true, Ordering::Release);
                Box::pin(async { Ok(faux_text_message("done")) })
            })),
        ]);
        let models = Models::default();
        models.set_provider(faux.provider);
        let mut options = HarnessOptions::new(registry.clone(), models.clone());
        options.default_document_retention_ms = retention;
        let context = Context::default();
        let storage = open_native_sqlite_storage(&database, Default::default())
            .await
            .unwrap();
        let harness = Harness::open(Arc::new(storage), options, &context)
            .await
            .unwrap();
        let root = harness
            .root(
                ConversationOptions {
                    agent: Some(
                        JsonObject::try_from_value(
                            pi_chord::json::parse(
                                r#"{"model":{"provider":"faux","modelId":"faux-1"}}"#,
                            )
                            .unwrap(),
                        )
                        .unwrap(),
                    ),
                    ..Default::default()
                },
                &context,
            )
            .await
            .unwrap();
        let conversation = root.id;
        let submission = root
            .submit(SubmissionDraft::input("run"), &context)
            .await
            .unwrap();
        let id = submission.id;
        tokio::time::timeout(std::time::Duration::from_secs(5), reached.notified())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), harness.close(&context))
            .await
            .unwrap()
            .unwrap();
        first_sandbox.0.close().await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(held.load(Ordering::Relaxed), 1);
        blocked.store(false, Ordering::Release);
        let second_sandbox = Arc::new(vm::NativeSandbox::new().unwrap());
        let mut code = CodemodeToolOptions::new(second_sandbox.clone());
        code.models = false;
        registry
            .install(Arc::new(create_codemode_extension(code)))
            .unwrap();
        let mut options = HarnessOptions::new(registry, models);
        options.default_document_retention_ms = retention;
        let storage = open_native_sqlite_storage(&database, Default::default())
            .await
            .unwrap();
        let harness = Harness::open(Arc::new(storage), options, &context)
            .await
            .unwrap();
        harness.resume().unwrap();
        let submission = harness.submission(id, &context).await.unwrap().unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            submission.wait(&context),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(checked.load(Ordering::Acquire));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(held.load(Ordering::Relaxed), 2);
        assert!(
            harness
                .snapshot(&CODEMODE_STORE_DOC, conversation, &context)
                .await
                .unwrap()
                .unwrap()
                .values
                .value()["done"]
                .string_equals("resumed")
        );
        harness.close(&context).await.unwrap();
        second_sandbox.0.close().await;
    }
}

#[tokio::test]
async fn native_sandbox_runs_durable_nested_calls_discovery_and_atomic_store_writes() {
    for retention in [600000.0, 0.0] {
        let sandbox = Arc::new(vm::NativeSandbox::new().unwrap());
        let registry = create_registry::<NativeTool, NativeSection>().unwrap();
        let mut options = CodemodeToolOptions::new(sandbox.clone());
        options.models = false;
        registry
            .install(Arc::new(create_codemode_extension(options)))
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut tools = Extension::<NativeTool, NativeSection>::new("test");
        tools.tools.push(Arc::new(NativeTool::new(ToolDeclaration::new("echo","Echo the text",pi_chord::json::parse(r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#).unwrap()),Arc::new(move |args,_,_| {
            count.fetch_add(1,Ordering::Relaxed);let mut text=JsonString::from("echo ");text.append(args.get(&JsonString::from("text")).unwrap().as_string().unwrap());
            Box::pin(async move {Ok(ToolExecutionResult {output:Some(Arc::new(vec![text_content(text)])),..Default::default()})})
        }))));
        registry.install(Arc::new(tools)).unwrap();
        let checked = Arc::new(AtomicBool::new(false));
        let seen = checked.clone();
        let faux = faux_provider(RegisterFauxProviderOptions {
            tokens_per_second: Some(f64::INFINITY),
            ..Default::default()
        });
        let code = r#"
            const a = await tools.echo({text: "first"});
            const b = await tools.echo({text: "second"});
            let rejected = 0;
            for (const invalid of [null, 42, []]) {
                try { await tools.echo(invalid); }
                catch (error) { if (error.message) rejected++; }
            }
            text(a);
            store("value", {answer: b});
            return [Date.now() > 0, typeof WeakRef,
                (await describeTool("echo")) !== undefined,
                (await searchTools("echo")).length, rejected];
        "#;
        let arguments = serde_json::json!({"code":code})
            .as_object()
            .unwrap()
            .clone();
        faux.set_responses(vec![
            faux_assistant_message(
                vec![faux_tool_call("codemode", arguments, Some("script"))],
                FauxMessageOptions {
                    stop_reason: Some(pi_ai::StopReason::ToolUse),
                    ..Default::default()
                },
            )
            .into(),
            FauxResponseStep::Factory(Arc::new(move |context, _, _, _| {
                let transcript = serde_json::to_value(context).unwrap();
                let receipt = transcript["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|m| m["role"] == "toolResult")
                    .unwrap();
                assert_eq!(receipt["isError"], false, "{receipt}");
                let text = receipt["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v["text"].as_str())
                    .collect::<String>();
                assert!(text.contains("echo first"), "{text}");
                assert!(text.contains(r#"[true,"undefined",true,1,3]"#), "{text}");
                seen.store(true, Ordering::Release);
                Box::pin(async { Ok(faux_text_message("done")) })
            })),
        ]);
        let models = Models::default();
        models.set_provider(faux.provider);
        let mut options = HarnessOptions::new(registry, models);
        options.default_document_retention_ms = retention;
        let context = Context::default();
        let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context)
            .await
            .unwrap();
        let root = harness
            .root(
                ConversationOptions {
                    agent: Some(
                        JsonObject::try_from_value(
                            pi_chord::json::parse(
                                r#"{"model":{"provider":"faux","modelId":"faux-1"}}"#,
                            )
                            .unwrap(),
                        )
                        .unwrap(),
                    ),
                    ..Default::default()
                },
                &context,
            )
            .await
            .unwrap();
        let submission = root
            .submit(SubmissionDraft::input("run"), &context)
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            submission.wait(&context),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(checked.load(Ordering::Acquire));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let store = harness
            .snapshot(&CODEMODE_STORE_DOC, root.id, &context)
            .await
            .unwrap()
            .unwrap();
        assert!(store.values.value()["value"]["answer"].string_equals("echo second"));
        harness.close(&context).await.unwrap();
        sandbox.0.close().await;
    }
}
