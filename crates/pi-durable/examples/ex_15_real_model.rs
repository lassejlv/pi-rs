//! Stream a real model's answer while it is generated. Skips cleanly without `OPENAI_API_KEY`.
//! Port of upstream test/examples/15-real-model.ts.
//!
//! `OPENAI_MODEL` overrides the upstream default model id (`gpt-6-sol`).
#[path = "ex_support/mod.rs"]
mod support;
use parking_lot::Mutex;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::*;
use std::{io::Write, sync::Arc};
use support::*;

/// Print only what is new since the last call; ignore frames that rewrite earlier text.
fn print_text(printed: &Mutex<String>, text: &str) {
    let mut printed = printed.lock();
    if text.len() <= printed.len() || !text.starts_with(printed.as_str()) {
        return;
    }
    print!("{}", &text[printed.len()..]);
    let _ = std::io::stdout().flush();
    *printed = text.to_string();
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var_os("OPENAI_API_KEY").is_none() {
        println!("skipped: OPENAI_API_KEY is not set");
        return Ok(());
    }
    let context = Context::default();
    let models = pi_ai::models::Models::default();
    models.set_provider(pi_ai::providers::builtin::openai_provider());
    let registry = create_registry::<NativeTool, NativeSection>()?;
    let mut concise = define_extension(Ext::new("concise"));
    concise.sections.push(Arc::new(section(
        "preamble",
        Arc::new(|_, _| Box::pin(async { Ok(Some("You are a concise assistant.".into())) })),
        SectionOptions { tag: Some(false) },
    )));
    registry.install(Arc::new(concise))?;
    let harness = Harness::open(
        Arc::new(MemoryStorage::new()),
        HarnessOptions::new(registry, models),
        &context,
    )
    .await?;
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-6-sol".into());
    let root = harness
        .root(
            ConversationOptions {
                agent: Some(obj(&format!(
                    r#"{{"model":{{"provider":"openai","modelId":"{model}"}},"thinkingLevel":"high"}}"#
                ))),
                ..Default::default()
            },
            &context,
        )
        .await?;

    // The live document carries the partial assistant message while it streams.
    let live_watch = harness
        .watch_doc(&*LIVE_DOC, root.id, &context)
        .await?
        .ok_or_else(|| Error::message("live document is absent"))?;
    let printed = Arc::new(Mutex::new(String::new()));
    let streamed = printed.clone();
    live_watch.start(move |frame| {
        let streamed = streamed.clone();
        async move {
            let block = frame
                .value
                .as_ref()
                .and_then(|live| live.generation.as_ref())
                .and_then(|generation| generation.message.as_ref())
                .and_then(|message| {
                    serde_json::to_value(message).ok()?["content"]
                        .as_array()?
                        .iter()
                        .find(|part| part["type"] == "text")
                        .and_then(|part| part["text"].as_str().map(str::to_string))
                });
            if let Some(text) = block {
                print_text(&streamed, &text);
            }
            Ok(())
        }
    })?;
    harness.resume()?;
    print!("answer: ");
    let poem = root
        .submit(SubmissionDraft::input("Write a long poem"), &context)
        .await?;
    let settled = poem.wait(&context).await?;
    live_watch.stop();
    match answer_text(&root, &settled, &context).await? {
        Some(text) => {
            print_text(&printed, &text);
            println!();
        }
        None => println!("unanswered: {:?}", settled.state),
    }
    harness.close(&context).await
}
