//! The inbox: what happens to submissions while a conversation is busy.
#[path = "ex_b_support/mod.rs"]
mod support;
use pi_durable::harness::inbox::InputMode;
use pi_durable::*;
use serde_json::json;
use std::sync::Arc;
use support::*;

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    // The first answer waits until we let it go, so the conversation stays busy while we submit more.
    let held = Arc::new(tokio::sync::Notify::new());
    let (models, faux) = faux(fast());
    faux.set_responses(vec![
        held_answer(held.clone(), "Answer to the first question."),
        answer("Answer to the follow-up and the steer."),
    ]);

    // Settings apply to every conversation: place every queued follow-up at once instead of one per run.
    let options = HarnessOptions::new(registry(), models).with_settings(HarnessSettings {
        follow_up_mode: Some("all".into()),
        ..Default::default()
    });
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context).await?;
    let root = harness
        .root(root_options("faux-1", json!({})), &context)
        .await?;

    let first = root
        .submit(SubmissionDraft::input("First question"), &context)
        .await?;

    // While busy, input queues as a follow-up (the default) or a steer, and writes queue too.
    let follow_up = root
        .submit(SubmissionDraft::input("A follow-up"), &context)
        .await?;
    let steer = root
        .submit(input_when("A steer", WhenBusy::Steer), &context)
        .await?;
    let mut draft = EntryDraft::new("app.note");
    draft.data = Some("noted while busy".into());
    let note = root.submit(SubmissionDraft::write(draft), &context).await?;
    let withdrawn = root
        .submit(SubmissionDraft::input("Never mind"), &context)
        .await?;
    // whenBusy: "reject" refuses instead of queueing.
    let rejected = root
        .submit(input_when("Now or never", WhenBusy::Reject), &context)
        .await;
    match &rejected {
        Err(error) => println!("rejected: {error}"),
        Ok(_) => {
            return Err(Error::message(
                "A reject submission must not queue while busy",
            ));
        }
    }
    // A queued submission can be withdrawn until a boundary places it.
    let withdraw = withdrawn.abort(&context).await?;
    println!("withdraw: {withdraw:?}");
    check(
        withdraw == SubmissionAbort::Aborted,
        "withdrawn while queued",
    )?;

    let inbox = harness.snapshot(&INBOX_DOC, root.id, &context).await?;
    let items: Vec<String> = inbox
        .iter()
        .flat_map(|inbox| inbox.items.iter())
        .map(|item| match item {
            InboxItem::Input { id, mode, .. } => {
                format!(
                    "{id} {}",
                    if *mode == InputMode::Steer {
                        "steer"
                    } else {
                        "followUp"
                    }
                )
            }
            InboxItem::Write { id, .. } => format!("{id} write"),
        })
        .collect();
    println!("inbox: {items:?}");
    check(
        items.len() == 2
            && !items
                .iter()
                .any(|item| item.starts_with(&withdrawn.id.to_string())),
        "the follow-up and the steer are queued; the withdrawn one is gone",
    )?;
    check(
        items.iter().any(|item| item.ends_with("steer")),
        "steer queued",
    )?;
    check(
        items
            .iter()
            .filter(|item| item.ends_with("followUp"))
            .count()
            == 1,
        "one follow-up queued",
    )?;

    // The first answer ends the run at a final boundary: the write is placed first, then the steer and the
    // follow-up, which start the next run together.
    held.notify_one();
    let mut statuses = Vec::new();
    for (name, submission) in [
        ("first", &first),
        ("follow-up", &follow_up),
        ("steer", &steer),
        ("note", &note),
        ("withdrawn", &withdrawn),
    ] {
        let record = submission.wait(&context).await?;
        let reason = unanswered_reason(&record).unwrap_or_default();
        println!("{name}: {:?} {reason}", status_of(&record));
        statuses.push((name, status_of(&record), reason));
    }
    for (name, status, _) in &statuses[..4] {
        check(*status == SubmissionStatus::Done, name)?;
    }
    check(
        statuses[4].1 == SubmissionStatus::Unanswered && statuses[4].2 == "aborted",
        "withdrawn is unanswered/aborted",
    )?;

    let kinds: Vec<String> = transcript(&root, 20)
        .await?
        .iter()
        .map(|entry| entry.kind.to_string())
        .collect();
    println!("transcript: {kinds:?}");
    check(
        kinds.iter().filter(|kind| *kind == "pi.user").count() == 3,
        "three user inputs placed",
    )?;
    check(
        kinds.iter().filter(|kind| *kind == "pi.assistant").count() == 2,
        "two answers",
    )?;
    check(kinds.iter().any(|kind| kind == "app.note"), "note placed")?;
    check(
        faux.core.state().call_count == 2,
        "the steer and follow-up shared one run",
    )?;
    harness.close(&context).await
}
