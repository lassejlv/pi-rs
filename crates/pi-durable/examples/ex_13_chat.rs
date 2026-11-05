//! A chat turn against a scripted offline model.
//! Port of upstream test/examples/13-chat.ts.
#[path = "ex_support/mod.rs"]
mod support;
use pi_ai::providers::faux::*;
use pi_durable::*;
use std::sync::Arc;
use support::*;

#[tokio::main]
async fn main() -> Result<()> {
    let context = Context::default();
    // The faux provider stands in for a real model (see ex_15_real_model).
    let (models, faux) = faux_models();
    faux.set_responses(vec![faux_text_message("Paris.").into()]);

    let registry = create_registry::<NativeTool, NativeSection>()?;
    let mut terse = define_extension(Ext::new("terse"));
    terse.sections.push(Arc::new(section(
        "preamble",
        Arc::new(|_, _| Box::pin(async { Ok(Some("You answer in one word.".into())) })),
        SectionOptions { tag: Some(false) },
    )));
    registry.install(Arc::new(terse))?;
    let harness = Harness::open(
        Arc::new(MemoryStorage::new()),
        HarnessOptions::new(registry, models),
        &context,
    )
    .await?;
    let root = harness.root(faux_agent(), &context).await?;

    // submit() durably admits user input and returns a Submission. The built-in pi.generation task
    // prepares the system prompt from the agent's sections, calls the model, and appends the
    // answer; wait() resolves once the input is answered or has failed.
    let capital = root
        .submit(SubmissionDraft::input("Capital of France?"), &context)
        .await?;
    let answered = capital.wait(&context).await?;
    let answer = answer_text(&root, &answered, &context).await?;
    println!("answer: {answer:?}");
    assert_eq!(answered.state.status(), SubmissionStatus::Done);
    assert_eq!(answer.as_deref(), Some("Paris."));

    // The transcript holds the user input, the positional system prompt, and the answer.
    let transcript = root
        .entries(ConversationEntryQuery::default(), 10, None, &context)
        .await?;
    let mut kinds: Vec<String> = transcript
        .items
        .iter()
        .map(|e| e.kind.to_string())
        .collect();
    kinds.reverse();
    println!("transcript: {kinds:?}");
    assert_eq!(kinds, ["pi.user", "pi.system", "pi.assistant"]);

    harness.close(&context).await
}
