//! Direct tests of the public inbox, live-state, usage and error helpers of the Harness.
//! Expected behaviour is taken from the upstream implementations in `harness/inbox.ts`, `live.ts`,
//! `usage.ts` and `tool.ts`.
use pi_durable::{
    harness::{
        inbox::*,
        live::{clear_progress, compaction_status, remove_compaction_status},
        tool::harness_error,
        types::{InputEntry, QueueMode, QueueModes},
        usage::{UsageBucket, record_usage},
    },
    *,
};
use std::sync::Arc;

fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
fn session() -> Session {
    Session::new(Arc::new(MemoryStorage::new()), SessionOptions::default())
}
fn modes() -> QueueModes {
    QueueModes {
        steering_mode: QueueMode::OneAtATime,
        follow_up_mode: QueueMode::OneAtATime,
    }
}

#[test]
fn display_only_means_no_model_head_or_edits_even_when_empty() {
    let mut entry = EntryDraft::new("note");
    assert!(is_display_only(&entry));
    entry.data = Some(json(r#"{"a":1}"#));
    assert!(is_display_only(&entry));
    let mut with_model = EntryDraft::new("note");
    with_model.model = Some(Vec::new());
    assert!(!is_display_only(&with_model));
    let mut with_head = EntryDraft::new("note");
    with_head.head = Some(EntryHead::ThisEntry);
    assert!(!is_display_only(&with_head));
    let mut with_edits = EntryDraft::new("note");
    with_edits.edits = Some(Vec::new());
    assert!(!is_display_only(&with_edits));
}

#[test]
fn a_queued_reset_is_a_write_with_a_self_head() {
    let write = |head| {
        let mut entry = EntryDraft::new("pi.reset");
        entry.head = head;
        InboxItem::Write {
            id: SubmissionId::from_raw(1),
            entry,
        }
    };
    let input = InboxItem::Input {
        id: SubmissionId::from_raw(2),
        mode: InputMode::Steer,
        content: json(r#""hi""#),
        entry: None,
    };
    assert!(!has_queued_reset(&[]));
    assert!(!has_queued_reset(&[input.clone(), write(None)]));
    assert!(!has_queued_reset(&[write(Some(EntryHead::Entry(
        EntryId::from_raw(3)
    )))]));
    assert!(has_queued_reset(&[
        input,
        write(Some(EntryHead::ThisEntry))
    ]));
}

#[tokio::test]
async fn a_head_write_before_the_active_range_is_stale() {
    let session = session();
    let context = Context::default();
    let conversation = session
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &context,
        )
        .await
        .unwrap();
    // No head marker yet: nothing is stale (boundaries read tables before any write).
    let before = session
        .commit(
            move |tx| async move {
                let open = prepare_boundary(&tx, conversation, modes()).await?;
                let mut older = EntryDraft::new("e");
                older.head = Some(EntryHead::Entry(EntryId::from_raw(1)));
                Ok(is_stale(&open, &older))
            },
            &context,
        )
        .await
        .unwrap();
    let marker = session
        .commit(
            move |tx| async move {
                let mut marker = EntryDraft::new("m");
                marker.head = Some(EntryHead::ThisEntry);
                tx.append_entry(conversation, marker).await
            },
            &context,
        )
        .await
        .unwrap();
    let stale = session
        .commit(
            move |tx| async move {
                let active = prepare_boundary(&tx, conversation, modes()).await?;
                let at = |id: i64| {
                    let mut entry = EntryDraft::new("e");
                    entry.head = Some(EntryHead::Entry(EntryId::from_raw(id)));
                    entry
                };
                let head = marker.id.get();
                Ok((
                    before,
                    is_stale(&active, &at(head - 1)),
                    is_stale(&active, &at(head)),
                    is_stale(&active, &at(head + 1)),
                    // A self head or no head is never stale.
                    is_stale(&active, &EntryDraft::new("e")),
                ))
            },
            &context,
        )
        .await
        .unwrap();
    assert_eq!(stale, (false, true, false, false, false));
}

#[tokio::test]
async fn input_entries_carry_a_user_message_and_inbox_items_are_removed_by_id() {
    let session = session();
    let context = Context::default();
    let (plain, custom, remaining) = session
        .commit(
            |tx| async move {
                let conversation = tx.create_root_conversation().await?.id;
                let plain = append_input(&tx, conversation, json(r#""hello""#), None, 42.0).await?;
                let mut entry = InputEntry::new("note");
                entry.data = Some(json(r#"{"k":1}"#));
                let custom =
                    append_input(&tx, conversation, json(r#""x""#), Some(&entry), 43.0).await?;
                let inbox = tx.doc(&inbox_doc(), conversation).await?;
                let input = |id: i64| InboxItem::Input {
                    id: SubmissionId::from_raw(id),
                    mode: InputMode::FollowUp,
                    content: json(r#""q""#),
                    entry: None,
                };
                push_inbox_item(&inbox, input(10))?;
                push_inbox_item(&inbox, input(11))?;
                remove_inbox_item(&tx, conversation, SubmissionId::from_raw(10)).await?;
                // Removing an unknown id changes nothing.
                remove_inbox_item(&tx, conversation, SubmissionId::from_raw(99)).await?;
                let remaining: Vec<i64> = inbox
                    .read()?
                    .items
                    .iter()
                    .map(|item| item.id().get())
                    .collect();
                Ok((plain, custom, remaining))
            },
            &context,
        )
        .await
        .unwrap();
    assert_eq!(plain.kind.to_string_lossy(), "pi.user");
    let message = serde_json::to_value(&plain.model.unwrap()[0]).unwrap();
    assert_eq!(
        message,
        serde_json::json!({ "role": "user", "content": "hello", "timestamp": 42 })
    );
    assert_eq!(custom.kind.to_string_lossy(), "note");
    assert_eq!(
        serde_json::to_value(custom.data.unwrap()).unwrap(),
        serde_json::json!({ "k": 1 })
    );
    assert_eq!(remaining, [11]);
}

#[tokio::test]
async fn usage_accumulates_per_bucket_and_key() {
    let session = session();
    let context = Context::default();
    let state = session
        .commit(
            |tx| async move {
                let conversation = tx.create_root_conversation().await?.id;
                let usage = |input: u32, output: u32, extra: &str| {
                    JsonObject::try_from_value(json(&format!(
                        r#"{{"input":{input},"output":{output},"cacheRead":0,"cacheWrite":0,"totalTokens":{},"cost":{{"input":0.5,"output":0.25,"cacheRead":0,"cacheWrite":0,"total":0.75}}{extra}}}"#,
                        input + output
                    )))
                    .unwrap()
                };
                record_usage(&tx, conversation, UsageBucket::Models, "faux/faux-1", &usage(3, 2, "")).await?;
                record_usage(&tx, conversation, UsageBucket::Models, "faux/faux-1", &usage(4, 1, r#","reasoning":7"#)).await?;
                record_usage(&tx, conversation, UsageBucket::Tools, "bash", &usage(1, 0, "")).await?;
                let doc = tx.doc(&usage_doc(), conversation).await?;
                let state = doc.read()?;
                Ok(state)
            },
            &context,
        )
        .await
        .unwrap();
    let models = &state.models[&JsonString::from("faux/faux-1")];
    // Counters add; the optional counter appears once either side reports it; costs add.
    assert_eq!(
        models.value(),
        &json(
            r#"{"input":7,"output":3,"cacheRead":0,"cacheWrite":0,"totalTokens":10,"cost":{"input":1,"output":0.5,"cacheRead":0,"cacheWrite":0,"total":1.5},"reasoning":7}"#
        )
    );
    assert_eq!(
        state.tools[&JsonString::from("bash")].value()["input"],
        json("1")
    );
    assert_eq!(state.tools.len(), 1);
}

#[test]
fn live_state_helpers_find_and_clear_by_task() {
    use pi_durable::harness::live::{CompactionReason, CompactionStatus, LiveState, SlotProgress};
    let status = |task: i64| CompactionStatus {
        task_id: TaskId::from_raw(task),
        reason: CompactionReason::Threshold,
        blocking: true,
        attempt: 1,
        retry: None,
    };
    let mut live = LiveState {
        compactions: Some(vec![status(1), status(2)]),
        ..Default::default()
    };
    assert_eq!(
        compaction_status(&mut live, TaskId::from_raw(2))
            .unwrap()
            .task_id
            .get(),
        2
    );
    assert!(compaction_status(&mut live, TaskId::from_raw(3)).is_none());
    remove_compaction_status(&mut live, TaskId::from_raw(1));
    assert_eq!(live.compactions.as_ref().unwrap().len(), 1);
    remove_compaction_status(&mut live, TaskId::from_raw(2));
    assert!(live.compactions.is_none(), "an empty list is dropped");
    assert!(compaction_status(&mut LiveState::default(), TaskId::from_raw(1)).is_none());

    let mut slot = SlotProgress::new("call-1", "bash");
    slot.output = Some("out".into());
    slot.dropped_bytes = Some(4);
    slot.dropped_lines = Some(1);
    slot.details = Some(json(r#"{"d":1}"#));
    slot.diagnostics = Some(Vec::new());
    clear_progress(&mut slot);
    assert!(slot.output.is_none() && slot.dropped_bytes.is_none() && slot.dropped_lines.is_none());
    assert!(slot.details.is_none() && slot.diagnostics.is_none());
    assert_eq!(slot.call_id.to_string_lossy(), "call-1");
}

#[test]
fn harness_errors_are_error_results_with_one_diagnostic() {
    let result = harness_error("not_found", "No such tool");
    assert_eq!(result.is_error, Some(true));
    assert_eq!(result.output.as_ref().map(|output| output.len()), Some(0));
    let diagnostics = result.diagnostics.unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].message.to_string_lossy(), "No such tool");
}
