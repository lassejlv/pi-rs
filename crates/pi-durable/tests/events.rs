use pi_chord::{
    AbortController, WatchEnd,
    context::with_abort_signal,
    delta::{Op, SharedJson},
};
use pi_durable::{
    harness::{events::*, live::LIVE_DOC, view::ConversationViews},
    *,
};
use std::sync::Arc;
use tokio::sync::mpsc;

fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
fn encode(value: &impl serde::Serialize) -> JsonValue {
    pi_chord::json::to_value(value).unwrap()
}
fn ops(text: &str) -> Vec<Op> {
    serde_json::from_str(text).unwrap()
}
fn publication(changes: Vec<CommitChange>) -> CommitPublication {
    CommitPublication {
        seq: Seq::from_raw(1),
        changes,
    }
}
fn translate(before: &SharedJson, operations: &[Op]) -> (SharedJson, EventBatch) {
    let after = before.apply(operations).unwrap();
    let batch = EventTranslator::default()
        .translate(
            ConversationId::from_raw(1),
            before,
            &after,
            operations,
            &publication(vec![]),
        )
        .unwrap();
    (after, batch)
}

#[test]
fn unrelated_live_edits_and_array_moves_do_not_resend_partial_or_details() {
    let before = SharedJson::new(json(
        r#"{"entries":[],"docs":{"pi.live":{"generation":{"attempt":1,"message":{"role":"assistant","content":[{"type":"text","text":"hello"}],"usage":{}}},"tools":[{"callId":"a","name":"echo","taskId":10,"status":"running","details":{"n":1}},{"callId":"b","name":"echo","taskId":11,"status":"running","details":{"n":2}}]}}}"#,
    ));
    let (moved, events) = translate(
        &before,
        &ops(
            r#"[["m",["docs","pi.live","tools"],[1,0]],["s",["docs","pi.live","tools",0,"droppedBytes"],10]]"#,
        ),
    );
    assert!(events.is_empty());
    assert!(
        before
            .get(&[
                "docs".into(),
                "pi.live".into(),
                "generation".into(),
                "message".into()
            ])
            .unwrap()
            .ptr_eq(
                moved
                    .get(&[
                        "docs".into(),
                        "pi.live".into(),
                        "generation".into(),
                        "message".into()
                    ])
                    .unwrap()
            )
    );
    let (after, events) = translate(
        &moved,
        &ops(r#"[["s",["docs","pi.live","tools",0,"details"],{"n":2}]]"#),
    );
    assert_eq!(
        encode(&events),
        json(
            r#"[{"type":"tool_execution_update","toolCallId":"b","toolName":"echo","taskId":11,"details":{"n":2}}]"#
        )
    );
    let AgentEvent::ToolExecutionUpdate { update, .. } = &events[0] else {
        panic!("expected details update")
    };
    assert!(
        update.details.as_ref().unwrap().ptr_eq(
            after
                .get(&[
                    "docs".into(),
                    "pi.live".into(),
                    "tools".into(),
                    0.into(),
                    "details".into()
                ])
                .unwrap()
        )
    );
}

#[test]
fn completing_outcome_ends_each_turn_once_even_after_reattachment() {
    let held: TaskRecord = serde_json::from_str(r#"{"id":2,"conversationId":1,"kind":"pi.generation","version":1,"input":null,"background":false,"abortRequested":false,"state":{"status":"completing","outcome":{"status":"completed","result":null}}}"#).unwrap();
    let root = SharedJson::new(json(r#"{"entries":[],"docs":{}}"#));
    let mut translator = EventTranslator::default();
    let translate = |translator: &mut EventTranslator, record: TaskRecord| {
        translator
            .translate(
                ConversationId::from_raw(1),
                &root,
                &root,
                &[],
                &publication(vec![CommitChange::Task(Box::new(record))]),
            )
            .unwrap()
    };
    assert!(matches!(
        translate(&mut translator, held.clone()).as_slice(),
        [AgentEvent::TurnEnd]
    ));
    assert!(translate(&mut translator, held.clone()).is_empty());
    let mut terminal = held.clone();
    terminal.state = TaskState::Terminal {
        outcome: TaskOutcome::Completed {
            result: JsonValue::Null,
        },
    };
    assert!(translate(&mut translator, terminal.clone()).is_empty());
    assert!(translate(&mut EventTranslator::new([held.id()]), terminal.clone()).is_empty());
    assert!(matches!(
        translate(&mut EventTranslator::default(), terminal).as_slice(),
        [AgentEvent::TurnEnd]
    ));
}

#[test]
fn message_changes_keep_utf16_deltas_and_coalesce_whole_blocks() {
    let message = SharedJson::new(json(
        r#"{"content":[{"type":"text","text":"changed"},{"type":"toolCall","arguments":{"query":"value"}}],"usage":{"input":1}}"#,
    ));
    let changes = message_changes(&ops(r#"[["a",["docs","pi.live","generation","message","content",0,"text"],"\ud800"],["s",["docs","pi.live","generation","message","content",0,"text"],"changed"],["a",["docs","pi.live","generation","message","content",0,"text"],"ignored"],["a",["docs","pi.live","generation","message","content",1,"arguments","query"],"value"],["s",["docs","pi.live","generation","message","usage","input"],1]]"#), &message).unwrap();
    assert_eq!(
        encode(&changes),
        json(
            r#"[{"type":"text_delta","contentIndex":0,"delta":"\ud800"},{"type":"block","contentIndex":0,"block":{"type":"text","text":"changed"}},{"type":"toolcall_delta","contentIndex":1,"path":["query"],"delta":"value"}]"#
        )
    );
    let MessageChange::Block { block, .. } = &changes[1] else {
        panic!("expected whole block")
    };
    assert!(block.ptr_eq(message.get(&["content".into(), 0.into()]).unwrap()));
}

async fn setup() -> (Session, ConversationViews, ConversationId) {
    let session = Session::new(
        Arc::new(MemoryStorage::new()),
        SessionOptions {
            conversation_created: Some(Arc::new(|tx, record| {
                Box::pin(async move {
                    pi_durable::harness::create_conversation_documents(&tx, record).await
                })
            })),
            ..Default::default()
        },
    );
    let root = session
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &Context::default(),
        )
        .await
        .unwrap();
    let views = ConversationViews::new(session.clone()).unwrap();
    (session, views, root)
}
#[tokio::test]
async fn overflow_captures_latest_partial_and_later_deltas_use_that_revision() {
    let (session, views, root) = setup().await;
    let stream = watch_events(&views, root, &Context::default())
        .await
        .unwrap();
    assert!(stream.snapshot.entries.as_array().unwrap().is_empty());
    for index in 0..101 {
        session.commit(move |tx| async move {
            tx.doc(&LIVE_DOC, root).await?.apply_ops(&[Op::Set(vec!["generation".into()], json(&format!(r#"{{"attempt":1,"message":{{"role":"assistant","content":[{{"type":"text","text":"{index}"}}],"usage":{{}}}}}}"#)))])?;
            Ok(())
        }, &Context::default()).await.unwrap();
    }
    let (send, mut received) = mpsc::unbounded_channel();
    stream
        .start(move |batch, context| {
            assert!(context.abort_signal().is_none());
            send.send(batch).unwrap();
            async { Ok(()) }
        })
        .unwrap();
    stream.watch.delivery_idle().await;
    let batch = received.recv().await.unwrap();
    let [AgentEvent::Snapshot(snapshot)] = batch.as_slice() else {
        panic!("overflow must replace queued deltas with one snapshot")
    };
    assert_eq!(
        snapshot
            .generation
            .as_ref()
            .unwrap()
            .get(&["message".into(), "content".into(), 0.into(), "text".into()])
            .unwrap()
            .to_value(),
        json(r#""100""#)
    );
    assert!(received.try_recv().is_err());
    session
        .commit(
            move |tx| async move {
                tx.doc(&LIVE_DOC, root).await?.apply_ops(&[Op::Append(
                    vec![
                        "generation".into(),
                        "message".into(),
                        "content".into(),
                        0.into(),
                        "text".into(),
                    ],
                    "!".into(),
                )])?;
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    stream.watch.delivery_idle().await;
    assert_eq!(
        encode(received.recv().await.unwrap().as_ref()),
        json(
            r#"[{"type":"message_update","usage":{},"changes":[{"type":"text_delta","contentIndex":0,"delta":"!"}]}]"#
        )
    );
    session.close(&Context::default()).await.unwrap();
    assert_eq!(stream.closed().await, WatchEnd::Ended);
}

#[tokio::test]
async fn cancellation_ends_only_its_stream_and_failed_listeners_detach_independently() {
    let (session, views, root) = setup().await;
    let controller = AbortController::new();
    let cancelled = watch_events(
        &views,
        root,
        &with_abort_signal(controller.signal(), &Context::default()),
    )
    .await
    .unwrap();
    let failed = watch_events(&views, root, &Context::default())
        .await
        .unwrap();
    let kept = watch_events(&views, root, &Context::default())
        .await
        .unwrap();
    failed
        .start(|_, _| async { Err("listener failed".into()) })
        .unwrap();
    let (send, mut batches) = mpsc::unbounded_channel();
    kept.start(move |events, _| {
        send.send(events).unwrap();
        async { Ok(()) }
    })
    .unwrap();
    controller.abort();
    assert_eq!(cancelled.closed().await, WatchEnd::Cancelled);
    for _ in 0..2 {
        session
            .commit(
                move |tx| async move { tx.append_entry(root, EntryDraft::new("note")).await },
                &Context::default(),
            )
            .await
            .unwrap();
        kept.watch.delivery_idle().await;
        assert!(matches!(
            batches.recv().await.unwrap().as_slice(),
            [AgentEvent::EntryAppended { .. }]
        ));
    }
    assert_eq!(
        failed.closed().await,
        WatchEnd::ListenerError {
            error: "listener failed".into()
        }
    );
    assert_eq!(kept.stop(), WatchEnd::Stopped);
    session.close(&Context::default()).await.unwrap();
}
