#[allow(dead_code)]
#[path = "support/session_storage.rs"]
mod support;
use pi_durable::{harness::context::*, *};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

fn json(value: &str) -> JsonValue {
    pi_chord::json::parse(value).unwrap()
}
fn user(text: &str) -> JsonValue {
    pi_chord::json::to_value(&serde_json::json!({"role":"user","content":text,"timestamp":0}))
        .unwrap()
}
async fn append(session: &Session, conversation: ConversationId, message: JsonValue) -> EntryId {
    session
        .commit(
            move |tx| async move {
                let mut draft = EntryDraft::new("message");
                draft.model = Some(vec![message]);
                Ok(tx.append_entry(conversation, draft).await?.id)
            },
            &Context::default(),
        )
        .await
        .unwrap()
}
async fn setup() -> (Arc<support::ControlledStorage>, Session, ConversationId) {
    let storage = Arc::new(support::ControlledStorage::default());
    let session = Session::new(storage.clone(), Default::default());
    let conversation = session
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &Context::default(),
        )
        .await
        .unwrap();
    (storage, session, conversation)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_context_read_queued_before_close_finishes_its_captured_transcript() {
    for _ in 0..128 {
        let (storage, session, conversation) = setup().await;
        append(&session, conversation, user("before close")).await;
        let gate = Arc::new(support::Gate::default());
        *storage.commit_gate.lock() = Some(gate.clone());
        let held = session.commit(
            move |tx| async move {
                tx.append_entry(conversation, EntryDraft::new("note"))
                    .await?;
                Ok(())
            },
            &Context::default(),
        );
        gate.entered().await;
        let read = read_context(&session, storage, conversation, &Context::default(), None);
        let closing = session.close(&Context::default());
        gate.release();
        held.await.unwrap();
        let view = read.await.unwrap();
        closing.await.unwrap();
        assert_eq!(view.entries.len(), 2);
        assert_eq!(*view.messages[0], user("before close"));
    }
}

// Upstream resumes a `readOnLine` caller before the next line job starts (the
// line's tail settles one reaction later), so a context read issues its range
// scan before later admitted work, including close, reaches Storage. A job
// admitted while the bounds capture runs must not overtake that continuation.
#[tokio::test]
async fn a_line_job_admitted_during_bounds_capture_starts_after_the_read_continues() {
    let (storage, session, conversation) = setup().await;
    append(&session, conversation, user("before")).await;
    let rows_at_commit = Arc::new(parking_lot::Mutex::new(None));
    let later = Arc::new(parking_lot::Mutex::new(None));
    *storage.head_hook.lock() = Some(Box::new({
        let (session, storage, rows_at_commit, later) = (
            session.clone(),
            storage.clone(),
            rows_at_commit.clone(),
            later.clone(),
        );
        move || {
            *later.lock() = Some(session.commit(
                move |_| async move {
                    *rows_at_commit.lock() = Some(storage.entry_rows.load(Ordering::SeqCst));
                    Ok(())
                },
                &Context::default(),
            ));
        }
    }));
    // Queue the read behind a held commit so its continuation is already waiting
    // on the bounds result when the bounds capture runs.
    let gate = Arc::new(support::Gate::default());
    *storage.commit_gate.lock() = Some(gate.clone());
    let held = session.commit(
        move |tx| async move {
            tx.append_entry(conversation, EntryDraft::new("note"))
                .await?;
            Ok(())
        },
        &Context::default(),
    );
    gate.entered().await;
    let read = read_context(
        &session,
        storage.clone(),
        conversation,
        &Context::default(),
        None,
    );
    tokio::task::yield_now().await;
    let rows_before = storage.entry_rows.load(Ordering::SeqCst);
    gate.release();
    held.await.unwrap();
    let view = read.await.unwrap();
    let later = later.lock().take().expect("hook admitted a later commit");
    later.await.unwrap();
    assert_eq!(view.entries.len(), 2);
    // Bounds capture reads one row; the range scan reads both transcript rows.
    assert_eq!(*rows_at_commit.lock(), Some(rows_before + 3));
}

#[tokio::test]
async fn native_session_captures_an_immutable_range_before_later_resets_and_edits() {
    let (storage, session, conversation) = setup().await;
    let first = append(&session, conversation, user("first")).await;
    let bounds = capture_context_bounds(storage.as_ref(), conversation, &Context::default(), None)
        .await
        .unwrap();
    session
        .commit(
            move |tx| async move {
                let mut draft = EntryDraft::new("reset");
                draft.head = Some(EntryHead::ThisEntry);
                draft.edits = Some(vec![ContextEdit::Omit { target: first }]);
                tx.append_entry(conversation, draft).await?;
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let view = derive_context(storage.as_ref(), conversation, bounds, &Context::default())
        .await
        .unwrap();
    assert_eq!(
        view.entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        [first]
    );
    assert_eq!(*view.messages[0], user("first"));
    assert!(
        read_context(
            &session,
            storage.clone(),
            conversation,
            &Context::default(),
            None
        )
        .await
        .unwrap()
        .messages
        .is_empty()
    );
    let fork = session
        .commit(
            move |tx| async move {
                tx.fork_conversation(conversation, first, ConversationOwnership::Ownerless)
                    .await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        read_context(
            &session,
            storage.clone(),
            fork.id,
            &Context::default(),
            None
        )
        .await
        .unwrap(),
        view
    );
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn incremental_context_reuses_message_identity_reads_only_new_rows_and_cuts_without_scanning()
{
    let (storage, session, conversation) = setup().await;
    let first = append(&session, conversation, user("one")).await;
    let initial = read_context_from(
        &session,
        storage.clone(),
        conversation,
        &Context::default(),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(storage.entry_rows.load(Ordering::SeqCst), 2);
    let old = initial.range.unwrap();
    append(&session, conversation, user("two")).await;
    let before = storage.entry_rows.load(Ordering::SeqCst);
    let extended = read_context_from(
        &session,
        storage.clone(),
        conversation,
        &Context::default(),
        None,
        Some(old.clone()),
    )
    .await
    .unwrap();
    assert_eq!(storage.entry_rows.load(Ordering::SeqCst) - before, 2);
    assert!(Arc::ptr_eq(
        &old.view.messages[0],
        &extended.view.messages[0]
    ));
    assert!(Arc::ptr_eq(
        &extended.view.messages[0],
        &extended.view.contributions[0][0]
    ));
    let before = storage.entry_rows.load(Ordering::SeqCst);
    let cut = read_context_from(
        &session,
        storage.clone(),
        conversation,
        &Context::default(),
        Some(first),
        extended.range,
    )
    .await
    .unwrap();
    assert_eq!(storage.entry_rows.load(Ordering::SeqCst), before);
    assert_eq!(cut.view, initial.view);
    session.close(&Context::default()).await.unwrap();
}

#[test]
fn tool_result_ordering_preserves_utf16_ids_and_uses_only_the_first_result_before_the_next_assistant()
 {
    let messages:Vec<Arc<JsonValue>>=serde_json::from_str(r#"[
      {"role":"assistant","timestamp":7,"content":[{"type":"toolCall","id":"\ud800","name":"\udfff"},{"type":"toolCall","id":"missing","name":"tool"}]},
      {"role":"toolResult","toolCallId":"\ud800","content":[],"isError":false},
      {"role":"toolResult","toolCallId":"\ud800","content":[],"isError":true},
      {"role":"assistant","timestamp":8,"content":[]},
      {"role":"toolResult","toolCallId":"missing","content":[],"isError":false}
    ]"#).unwrap();
    let ordered = order_tool_results(&messages);
    assert_eq!(ordered.len(), 4);
    assert!(Arc::ptr_eq(&ordered[1], &messages[1]));
    assert_eq!(
        ordered[2]["details"],
        json(r#"{"reason":"missing_result"}"#)
    );
    assert_eq!(ordered[2]["timestamp"], json("7"));
    assert!(Arc::ptr_eq(&ordered[3], &messages[3]));
}

#[tokio::test]
async fn context_range_scans_run_outside_the_session_line() {
    let (storage, session, conversation) = setup().await;
    append(&session, conversation, user("before")).await;
    let gate = Arc::new(support::Gate::default());
    *storage.range_gate.lock() = Some(gate.clone());
    let read = read_context(
        &session,
        storage.clone(),
        conversation,
        &Context::default(),
        None,
    );
    let read = tokio::spawn(read);
    gate.entered().await;
    tokio::time::timeout(
        Duration::from_secs(1),
        append(&session, conversation, user("after")),
    )
    .await
    .expect("range scan held the Session line");
    gate.release();
    let view = read.await.unwrap().unwrap();
    assert_eq!(view.entries.len(), 1);
    assert_eq!(*view.messages[0], user("before"));
    session.close(&Context::default()).await.unwrap();
}
