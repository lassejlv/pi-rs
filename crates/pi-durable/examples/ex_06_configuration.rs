//! Per-conversation agent choices and Harness-wide settings.
//! Port of upstream test/examples/06-configuration.ts.
#[path = "ex_support/mod.rs"]
mod support;
use parking_lot::Mutex;
use pi_durable::*;
use std::sync::Arc;
use support::*;

fn extension(name: &str, tools: Vec<Arc<NativeTool>>) -> Arc<Ext> {
    let mut extension = define_extension(Ext::new(name));
    extension.tools = tools;
    Arc::new(extension)
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = Context::default();
    let files = extension(
        "files",
        vec![
            path_tool("read", "Read a file"),
            path_tool("write", "Write a file"),
        ],
    );
    let search = extension("search", vec![path_tool("grep", "Search files")]);
    // Apps may attach their own metadata to tools, such as a prompt snippet. Sections see the
    // offered tools, so one can render their snippets.
    let mut snippets = define_extension(Ext::new("snippets"));
    snippets.sections.push(Arc::new(section(
        "tool_snippets",
        Arc::new(|input, _| {
            Box::pin(async move {
                let lines: Vec<String> = input
                    .agent
                    .tools
                    .iter()
                    .filter_map(|tool| tool.metadata.as_ref())
                    .filter_map(|meta| meta.get(&JsonString::from("snippet")).map(text))
                    .collect();
                Ok(Some(lines.join("\n").into()))
            })
        }),
        SectionOptions::default(),
    )));
    let registry = create_registry::<NativeTool, NativeSection>()?;
    registry.install(files.clone())?;
    registry.install(search.clone())?;
    registry.install(Arc::new(snippets))?;
    let (models, faux) = faux_models();
    faux.set_responses(vec![pi_ai::providers::faux::faux_text_message("ok").into()]);

    // Settings are Harness-wide run policy, read at every use and never stored.
    // A closure over shared state makes a value live.
    let timeout_ms = Arc::new(Mutex::new(60_000.0));
    let live = timeout_ms.clone();
    let mut options = HarnessOptions::new(registry.clone(), models);
    options.settings = Arc::new(move || {
        Ok(HarnessSettings {
            stream: Some(obj(&format!(r#"{{"timeoutMs":{}}}"#, *live.lock()))),
            retry: Some(obj(r#"{"maxRetries":5}"#)),
            tool_execution: Some("sequential".into()),
            ..Default::default()
        })
    });
    let settings = options.settings.clone();
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context).await?;
    let root = harness
        .root(ConversationOptions::default(), &context)
        .await?;

    let tools = || async { tool_names(&root.agent(&context).await?) };

    // A new conversation stores no choices and follows the host: every installed extension.
    let default_tools = tools().await?;
    println!("default tools: {default_tools:?}");
    assert_eq!(default_tools, ["read", "write", "grep"]);

    // configure() stores choices in the conversation's pi.agent document.
    root.configure(
        obj(r#"{
            "model":{"provider":"anthropic","modelId":"claude-sonnet-4-5"},
            "thinkingLevel":"high",
            "tools":[{"name":"write"},{"name":"read"}]
        }"#),
        &context,
    )
    .await?;
    let stored = harness
        .snapshot(&AGENT_DOC, root.id, &context)
        .await?
        .unwrap();
    println!("stored: {}", stored.0.value());
    assert_eq!(
        stored.0.value().to_string(),
        r#"{"model":{"provider":"anthropic","modelId":"claude-sonnet-4-5"},"thinkingLevel":"high","tools":["write","read"]}"#
    );
    let agent = root.agent(&context).await?;
    let selected = tool_names(&agent)?;
    println!(
        "model: {} thinking: {} tools: {selected:?}",
        agent.model().unwrap(),
        agent.thinking_level()
    );
    assert_eq!(agent.thinking_level().to_string(), "high");
    assert_eq!(
        agent.model().unwrap().to_string(),
        r#"{"provider":"anthropic","modelId":"claude-sonnet-4-5"}"#
    );
    let mut sorted = selected.clone();
    sorted.sort();
    assert_eq!(sorted, ["read", "write"]);

    // Deselect an extension; `null` clears a stored field back to the host default.
    root.configure(
        obj(r#"{"extensions":{"remove":[{"name":"search"}]},"tools":null}"#),
        &context,
    )
    .await?;
    let without_search = tools().await?;
    println!("without search: {without_search:?}");
    assert_eq!(without_search, ["read", "write"]);

    // Stored names outlive the code: uninstalling "files" leaves it selected, and requests
    // just stop offering its tools until it is back.
    root.configure(
        obj(r#"{"extensions":[{"name":"files"},{"name":"search"}]}"#),
        &context,
    )
    .await?;
    registry.uninstall(&"files".into())?;
    let uninstalled = tools().await?;
    println!("files uninstalled: {uninstalled:?}");
    assert_eq!(uninstalled, ["grep"]);
    registry.install(files)?;
    let reinstalled = tools().await?;
    println!("files reinstalled: {reinstalled:?}");
    assert_eq!(reinstalled, ["read", "write", "grep"]);

    // Settings changes need no commit; the next use reads the new timeout.
    assert_eq!(
        settings()?.stream.unwrap().value().to_string(),
        r#"{"timeoutMs":60000}"#
    );
    *timeout_ms.lock() = 120_000.0;
    assert_eq!(
        settings()?.stream.unwrap().value().to_string(),
        r#"{"timeoutMs":120000}"#
    );

    // The snippets section sees the offered tools' metadata when a request is prepared.
    let chat = harness
        .create_conversation(
            ConversationCreateOptions {
                agent: Some(obj(r#"{"model":{"provider":"faux","modelId":"faux-1"}}"#)),
                ..ConversationCreateOptions::new(ConversationOwnership::Ownerless)
            },
            &context,
        )
        .await?;
    chat.submit(SubmissionDraft::input("hi"), &context)
        .await?
        .wait(&context)
        .await?;
    let system = chat
        .context(None, &context)
        .await?
        .messages
        .iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .find(|m| m["role"] == "system")
        .expect("a system message");
    println!("tool_snippets: {}", system["sections"]["tool_snippets"]);
    assert_eq!(
        system["sections"]["tool_snippets"],
        // "files" was reinstalled above, so it now follows "search" in installation order.
        "<tool_snippets>\nUse grep for files.\nUse read for files.\nUse write for files.\n</tool_snippets>"
    );

    harness.close(&context).await
}
