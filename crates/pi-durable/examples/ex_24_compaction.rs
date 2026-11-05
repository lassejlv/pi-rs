//! Compaction: a long trip-planning chat whose older messages are summarized so the model context stays small.
#[path = "ex_b_support/mod.rs"]
mod support;
use parking_lot::Mutex;
use pi_ai::providers::faux::{
    FauxMessageOptions, FauxModelDefinition, FauxResponseStep, RegisterFauxProviderOptions,
    faux_assistant_message, faux_text, faux_text_message,
};
use pi_durable::*;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use support::*;

#[derive(Default)]
struct Script {
    overflow_once: AtomicBool,
    summaries: AtomicU64,
    /// While set, the next chat answer waits for it, which keeps the conversation busy.
    hold: Mutex<Option<Arc<tokio::sync::Notify>>>,
}

fn last_user_text(transcript: &serde_json::Value) -> String {
    transcript["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message["role"] == "user")
        })
        .map_or_else(String::new, |message| {
            content_text(&message["content"]).replace('\n', " ")
        })
}

/// A fake model with a tiny 3000-token window. It answers chat messages, writes summaries when asked to
/// summarize, and once rejects a request as too long, the way real providers report a context overflow.
fn respond(script: Arc<Script>) -> FauxResponseStep {
    FauxResponseStep::Factory(Arc::new(move |transcript, _, _, _| {
        let script = script.clone();
        let transcript = serde_json::to_value(transcript).expect("transcript JSON");
        Box::pin(async move {
            let first = &transcript["messages"][0];
            if first["role"] == "system"
                && content_text(&first["content"]).contains("summarization")
            {
                let number = script.summaries.fetch_add(1, Ordering::SeqCst) + 1;
                return Ok(faux_text_message(format!(
                    "## Goal\nPlan a week in Lisbon (summary #{number})."
                )));
            }
            if script.overflow_once.swap(false, Ordering::SeqCst) {
                return Ok(faux_assistant_message(
                    vec![faux_text("")],
                    FauxMessageOptions {
                        stop_reason: Some(pi_ai::StopReason::Error),
                        error_message: Some("prompt is too long".into()),
                        ..Default::default()
                    },
                ));
            }
            let held = script.hold.lock().take();
            if let Some(held) = held {
                held.notified().await;
            }
            let question = last_user_text(&transcript);
            Ok(faux_text_message(format!(
                "A detailed answer to \"{question}\": {}",
                "details ".repeat(200)
            )))
        })
    }))
}

/// Print the model context: one line per message, and the number of stored entries behind it. Returns the head
/// compaction reason, if the context starts with a summary.
async fn show(conversation: &Conversation, label: &str) -> Result<(usize, usize, Option<String>)> {
    let view = conversation.context(None, &ctx()).await?;
    let stored = transcript(conversation, 1000).await?.len();
    println!(
        "\n{label}: {} messages in context, {stored} entries stored",
        view.messages.len()
    );
    let reason = view
        .head
        .as_ref()
        .filter(|head| is_kind(head, "pi.compaction"))
        .and_then(|head| head.data.as_ref())
        .and_then(|data| sj(data)["reason"].as_str().map(str::to_string));
    if let Some(reason) = &reason {
        println!("  ({reason} compaction summary first)");
    }
    for message in &view.messages {
        let message = sj(message);
        let role = message["role"].as_str().unwrap_or("");
        let text = if role == "system" {
            "(system prompt)".to_string()
        } else {
            content_text(&message["content"])
                .replace('\n', " ")
                .chars()
                .take(70)
                .collect()
        };
        println!("  {role:<9} {text}");
    }
    Ok((view.messages.len(), stored, reason))
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    let script = Arc::new(Script::default());
    let (models, faux) = faux(RegisterFauxProviderOptions {
        models: vec![FauxModelDefinition {
            id: "tiny".into(),
            context_window: Some(3000),
            max_tokens: Some(1000),
            ..Default::default()
        }],
        tokens_per_second: Some(f64::INFINITY),
        ..Default::default()
    });
    faux.set_responses((0..100).map(|_| respond(script.clone())).collect());

    // Generation blocks to compact above 3000 - 1000 = 2000 tokens and starts a background compaction above
    // 2000 - 800. Settings are read at every use, so the reader makes `backgroundTokens` live.
    let background_tokens = Arc::new(AtomicU64::new(800));
    let live = background_tokens.clone();
    let mut options = HarnessOptions::new(registry(), models);
    options.settings = Arc::new(move || {
        Ok(HarnessSettings {
            compaction: Some(object(&format!(
                r#"{{"reserveTokens":1000,"keepRecentTokens":400,"backgroundTokens":{}}}"#,
                live.load(Ordering::SeqCst)
            ))),
            ..Default::default()
        })
    });
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context).await?;
    let root = harness
        .root(root_options("tiny", json!({})), &context)
        .await?;

    let ask = |question: &'static str| {
        let (root, harness) = (root.clone(), harness.clone());
        async move {
            let ctx = ctx();
            let record = root
                .submit(SubmissionDraft::input(question), &ctx)
                .await?
                .wait(&ctx)
                .await?;
            // Let a background compaction started by this turn finish, so its summary shows below.
            let running = harness
                .snapshot(&LIVE_DOC, root.id, &ctx)
                .await?
                .and_then(|live| live.compactions.clone())
                .unwrap_or_default();
            for status in running {
                harness.wait_for_task(status.task_id, &ctx).await?;
            }
            let outcome = if status_of(&record) == SubmissionStatus::Done {
                "answered".to_string()
            } else {
                unanswered_reason(&record).unwrap_or_default()
            };
            show(&root, &format!("after \"{question}\" ({outcome})")).await
        }
    };

    // 1. A long chat: once the context crosses the background threshold, a compaction runs while the chat goes
    // on, and its summary lands at once when the conversation is idle, otherwise at the next turn boundary.
    let mut compacted = false;
    let mut last = (0, 0, None);
    for question in [
        "Where should we stay?",
        "What should we eat?",
        "Which day trips?",
        "Any museums?",
        "Nightlife?",
    ] {
        last = ask(question).await?;
        compacted |= last.2.is_some();
    }
    check(compacted, "the long chat produced a compaction summary")?;
    check(
        last.0 < last.1,
        "the model context is smaller than the stored transcript",
    )?;
    check(
        script.summaries.load(Ordering::SeqCst) >= 1,
        "the model summarized",
    )?;

    // 2. A manual compaction while an answer is still being written. The summary is ready first, waits in the
    // inbox, and is placed right after the answer.
    let finish_answer = Arc::new(tokio::sync::Notify::new());
    *script.hold.lock() = Some(finish_answer.clone());
    let busy = root
        .submit(SubmissionDraft::input("How do we get around?"), &context)
        .await?;
    let manual = root
        .compact(Some("Keep the hotel shortlist".into()), &context)
        .await?;
    let task = harness.wait_for_task(manual.untyped(), &context).await?;
    let placement = match &task.state {
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { result },
        } => sj(result)["submissionId"]
            .as_i64()
            .map(SubmissionId::from_raw),
        _ => None,
    };
    let summary = harness
        .submission(
            placement.ok_or_else(|| Error::message("The manual compaction has no submission"))?,
            &context,
        )
        .await?
        .ok_or_else(|| Error::message("The summary submission is missing"))?;
    let before = summary.status(&context).await?.state.status();
    println!("\nmanual compaction finished; its summary is {before:?}");
    check(
        before == SubmissionStatus::Queued,
        "the summary waits in the inbox while the answer is written",
    )?;
    finish_answer.notify_one();
    busy.wait(&context).await?;
    let after = status_of(&summary.wait(&context).await?);
    println!("after the answer, the summary is {after:?}");
    check(
        after == SubmissionStatus::Done,
        "the summary is placed after the answer",
    )?;
    let (_, _, reason) = show(&root, "after compact()").await?;
    check(
        reason.as_deref() == Some("manual"),
        "manual compaction summary first",
    )?;

    // 3. The provider rejects a request as too long. Background compaction is turned off. By now the context is
    // over the blocking threshold, so generation compacts (reason "threshold") before the request, and the
    // one-shot rejection then meets a generation that already compacted: the run ends with a model error. The
    // upstream example behaves identically (verified by running it); the estimator and thresholds match.
    background_tokens.store(0, Ordering::SeqCst);
    ask("What should we pack?").await?;
    script.overflow_once.store(true, Ordering::SeqCst);
    let (_, _, reason) = ask("Summarize the plan for my partner").await?;
    check(
        reason.as_deref() == Some("threshold"),
        "the blocking threshold compacted before the request",
    )?;
    let unanswered = transcript(&root, 1000)
        .await?
        .iter()
        .filter(|entry| is_kind(entry, "pi.assistant"))
        .map(message_text)
        .all(|text| {
            !text.starts_with("A detailed answer to \"Summarize the plan for my partner\"")
        });
    check(unanswered, "the rejected request stayed unanswered")?;
    harness.close(&context).await
}
