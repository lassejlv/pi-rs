//! Golden (differential) tests of the text the model sees, against the TypeScript reference: the
//! system prompt, the built-in tool declarations, the compaction and branch summary requests, and
//! the CLI help. `tests/fixtures/golden/prompts.json` is written by `scripts/golden/gen-prompts.mjs`
//! (regenerate with `PI_TS_REF=<pi checkout> scripts/golden/run.sh scripts/golden/gen-prompts.mjs`).
//! Every comparison is exact, including JSON key order. `GOLDEN_FILTER=<substring>` limits a run.

use std::sync::{Arc, Once};

use parking_lot::Mutex;
use pi_agent::types::{AgentMessage, StreamFn, ThinkingLevel};
use pi_ai::types::{AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Model};
use pi_coding_agent::cli::args::help_text;
use pi_coding_agent::core::compaction::{
    CompactionPreparation, CompactionSettings, GenerateBranchSummaryOptions, compact, create_file_ops,
    format_file_operations, generate_branch_summary, generate_summary_with_usage, serialize_conversation,
};
use pi_coding_agent::core::extensions::types::ExtensionFlag;
use pi_coding_agent::core::messages::convert_to_llm;
use pi_coding_agent::core::session_manager::SessionEntry;
use pi_coding_agent::core::skills::{FormatSkillsForPromptFileReadTool, Skill, format_skills_for_prompt};
use pi_coding_agent::core::system_prompt::{BuildSystemPromptOptions, build_system_prompt, build_system_prompt_sections};
use pi_coding_agent::core::tools::create_all_tool_definitions;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const PACKAGE_DIR: &str = "/home/golden/pi-package";

#[derive(Debug, Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    kind: String,
    input: Value,
    out: Value,
}

fn fixture() -> Fixture {
    static ENV: Once = Once::new();
    ENV.call_once(|| {
        // SAFETY: set before any case runs; this test binary only reads it.
        unsafe {
            std::env::set_var("PI_PACKAGE_DIR", PACKAGE_DIR);
            std::env::set_var("FORCE_COLOR", "3");
            std::env::remove_var("NO_COLOR");
        }
    });
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/golden/prompts.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn selected<'a>(fixture: &'a Fixture, kinds: &[&str]) -> Vec<&'a Case> {
    let filter = std::env::var("GOLDEN_FILTER").unwrap_or_default();
    fixture
        .cases
        .iter()
        .filter(|c| kinds.contains(&c.kind.as_str()) && c.name.contains(&filter))
        .collect()
}

/// Line diff for readable failures.
fn diff(expected: &str, actual: &str) -> String {
    let e: Vec<&str> = expected.split('\n').collect();
    let a: Vec<&str> = actual.split('\n').collect();
    let mut out = String::new();
    for i in 0..e.len().max(a.len()) {
        let (x, y) = (e.get(i).copied(), a.get(i).copied());
        if x != y {
            out.push_str(&format!("line {}:\n  ts: {:?}\n  rs: {:?}\n", i + 1, x, y));
        }
    }
    out
}

fn check(failures: &mut Vec<String>, name: &str, expected: &str, actual: &str) {
    if expected != actual {
        failures.push(format!("{name}\n{}", diff(expected, actual)));
    }
}

fn check_json(failures: &mut Vec<String>, name: &str, expected: &Value, actual: &Value) {
    let e = serde_json::to_string_pretty(expected).unwrap();
    let a = serde_json::to_string_pretty(actual).unwrap();
    check(failures, name, &e, &a);
}

fn finish(failures: Vec<String>) {
    if !failures.is_empty() {
        panic!("{} golden case(s) differ:\n\n{}", failures.len(), failures.join("\n"));
    }
}

#[test]
fn golden_tool_definitions() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in selected(&fixture, &["tool_definition"]) {
        let cwd = case.input["cwd"].as_str().unwrap();
        let name = case.input["toolName"].as_str().unwrap();
        let defs = create_all_tool_definitions(cwd, None);
        let def = defs
            .iter()
            .find(|(tool, _)| serde_json::to_value(tool).unwrap() == json!(name))
            .map(|(_, def)| def)
            .unwrap_or_else(|| panic!("no tool {name}"));
        let mut actual = serde_json::Map::new();
        actual.insert("name".into(), json!(def.name));
        actual.insert("label".into(), json!(def.label));
        actual.insert("description".into(), json!(def.description));
        let mut put = |key: &str, value: Value| {
            if !value.is_null() {
                actual.insert(key.into(), value);
            }
        };
        put("promptSnippet", json!(def.prompt_snippet));
        put("promptGuidelines", json!(def.prompt_guidelines));
        put("parameters", def.parameters.clone());
        put("outputSchema", json!(def.output_schema));
        put("annotations", json!(def.annotations));
        put("executionMode", json!(def.execution_mode));
        put("exposure", json!(def.exposure));
        put("defaultActive", json!(def.default_active));
        put("renderShell", json!(def.render_shell));
        check_json(&mut failures, &case.name, &case.out, &Value::Object(actual));
    }
    finish(failures);
}

#[test]
fn golden_system_prompts() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in selected(&fixture, &["system_prompt"]) {
        let options: BuildSystemPromptOptions = serde_json::from_value(case.input.clone()).unwrap();
        let text = build_system_prompt(&options).unwrap();
        check(&mut failures, &case.name, case.out["text"].as_str().unwrap(), &text);
        if !case.out["sections"].is_null() {
            let sections = build_system_prompt_sections(&options).unwrap();
            check_json(&mut failures, &format!("{} (sections)", case.name), &case.out["sections"], &json!(sections));
        }
    }
    for case in selected(&fixture, &["format_skills"]) {
        let skills: Vec<Skill> = serde_json::from_value(case.input["skills"].clone()).unwrap();
        let tool: FormatSkillsForPromptFileReadTool = serde_json::from_value(case.input["fileReadTool"].clone()).unwrap();
        let text = format_skills_for_prompt(&skills, Some(tool));
        check(&mut failures, &case.name, case.out["text"].as_str().unwrap(), &text);
    }
    finish(failures);
}

/// Help lines that differ from TypeScript on purpose: `(TS line, Rust line)`.
const HELP_DIVERGENCES: &[(&str, &str)] = &[
    // `--tools +codemode` / `+tool_search` load those built-in extensions even with `-ne`
    // (`requested_builtin_tool_extensions` in `pi_main.rs`).
    (
        "  --no-extensions, -ne           Disable extension discovery and built-in extensions (explicit -e paths still work)",
        "  --no-extensions, -ne           Disable extension discovery and built-in extensions (explicit -e and --tools requests still work)",
    ),
];

#[test]
fn golden_help() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in selected(&fixture, &["help"]) {
        let flags: Option<Vec<ExtensionFlag>> = serde_json::from_value(case.input["flags"].clone()).unwrap();
        let text = help_text(flags.as_deref());
        let mut expected = case.out["text"].as_str().unwrap().to_string();
        for (ts, rs) in HELP_DIVERGENCES {
            // A divergence that no longer matches the TS text must be revisited, not ignored.
            assert!(expected.contains(ts), "{}: TS help no longer has {ts:?}", case.name);
            expected = expected.replace(ts, rs);
        }
        // `console.log` adds the trailing newline that `print_help`'s `println!` adds.
        check(&mut failures, &case.name, &expected, &text);
    }
    finish(failures);
}

fn messages() -> Vec<AgentMessage> {
    let fixture = fixture();
    let case = fixture.cases.iter().find(|c| c.kind == "serialize_conversation").unwrap();
    serde_json::from_value(case.input["messages"].clone()).unwrap()
}

fn model() -> Model {
    serde_json::from_value(json!({
        "id": "summarizer",
        "name": "Summarizer",
        "api": "anthropic-messages",
        "provider": "anthropic",
        "baseUrl": "https://api.anthropic.com",
        "reasoning": true,
        "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 200000,
        "maxTokens": 8192
    }))
    .unwrap()
}

fn strip_timestamps(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.shift_remove("timestamp");
            for v in map.values_mut() {
                strip_timestamps(v);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_timestamps),
        _ => {}
    }
}

/// Records each request (as the TS recorder does) and answers `"<reply> <n>"`.
fn recorder(reply: &'static str) -> (StreamFn, Arc<Mutex<Vec<Value>>>) {
    let calls: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let stream_fn = pi_agent::types::stream_fn(move |model, context, options| {
        let mut context = serde_json::to_value(&context).unwrap();
        strip_timestamps(&mut context);
        let n = {
            let mut calls = recorded.lock();
            calls.push(json!({
                "context": context,
                "reasoning": options.reasoning,
                "cacheRetention": options.cache_retention,
            }));
            calls.len()
        };
        let mut message = AssistantMessage::empty_for(&model, 0);
        message.content = serde_json::from_value(json!([{ "type": "text", "text": format!("{reply} {n}") }])).unwrap();
        message.usage = serde_json::from_value(json!({
            "input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 15,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 }
        }))
        .unwrap();
        let (sender, stream) = AssistantMessageEventStream::new();
        sender.push(AssistantMessageEvent::Done { reason: message.stop_reason, message });
        stream
    });
    (stream_fn, calls)
}

fn thinking(input: &Value) -> Option<ThinkingLevel> {
    input["thinkingLevel"].as_str().and_then(ThinkingLevel::parse)
}

fn without_usage(mut value: Value) -> Value {
    if let Value::Object(map) = &mut value {
        map.shift_remove("usage");
    }
    value
}

#[tokio::test]
async fn golden_compaction_requests() {
    let fixture = fixture();
    let messages = messages();
    let model = model();
    let mut failures = Vec::new();
    for case in selected(&fixture, &["serialize_conversation", "format_file_operations", "generate_summary", "compact"]) {
        let input = &case.input;
        match case.kind.as_str() {
            "serialize_conversation" => {
                let text = serialize_conversation(&convert_to_llm(&messages));
                check(&mut failures, &case.name, case.out["text"].as_str().unwrap(), &text);
            }
            "format_file_operations" => {
                let read: Vec<String> = serde_json::from_value(input["readFiles"].clone()).unwrap();
                let modified: Vec<String> = serde_json::from_value(input["modifiedFiles"].clone()).unwrap();
                check(&mut failures, &case.name, case.out["text"].as_str().unwrap(), &format_file_operations(&read, &modified));
            }
            "generate_summary" => {
                let (stream_fn, calls) = recorder("SUMMARY");
                let result = generate_summary_with_usage(
                    &messages,
                    &model,
                    None,
                    Some("key"),
                    None,
                    None,
                    input["customInstructions"].as_str(),
                    input["previousSummary"].as_str(),
                    thinking(input),
                    Some(stream_fn),
                    None,
                    None,
                    None,
                    Some("session-1"),
                )
                .await
                .unwrap();
                let actual = json!({ "calls": *calls.lock(), "text": result.text });
                check_json(&mut failures, &case.name, &case.out, &actual);
            }
            "compact" => {
                let (stream_fn, calls) = recorder("SUMMARY");
                let mut file_ops = create_file_ops();
                for (key, set) in [("read", &mut file_ops.read), ("edited", &mut file_ops.edited), ("written", &mut file_ops.written)] {
                    for f in input[key].as_array().into_iter().flatten() {
                        set.insert(f.as_str().unwrap().to_string());
                    }
                }
                let history = input["history"].as_bool().unwrap_or(false);
                let split = input["split"].as_bool().unwrap_or(false);
                let preparation = CompactionPreparation {
                    first_kept_entry_id: "keep-1".into(),
                    messages_to_summarize: if history { messages[..7].to_vec() } else { Vec::new() },
                    turn_prefix_messages: if split { messages[7..].to_vec() } else { Vec::new() },
                    is_split_turn: split,
                    tokens_before: 5000,
                    previous_summary: input["previousSummary"].as_str().map(str::to_string),
                    file_ops,
                    settings: CompactionSettings { enabled: true, reserve_tokens: 16384, keep_recent_tokens: 20000 },
                };
                let result = compact(
                    &preparation,
                    &model,
                    Some("key"),
                    None,
                    input["customInstructions"].as_str(),
                    None,
                    thinking(input),
                    Some(stream_fn),
                    None,
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
                let actual = json!({ "calls": *calls.lock(), "result": without_usage(serde_json::to_value(&result).unwrap()) });
                check_json(&mut failures, &case.name, &case.out, &actual);
            }
            _ => unreachable!(),
        }
    }
    finish(failures);
}

#[tokio::test]
async fn golden_branch_summary_requests() {
    let fixture = fixture();
    let messages = messages();
    let mut entries: Vec<SessionEntry> = messages
        .iter()
        .enumerate()
        .map(|(i, message)| {
            serde_json::from_value(json!({
                "type": "message",
                "id": format!("e{i}"),
                "parentId": if i == 0 { Value::Null } else { json!(format!("e{}", i - 1)) },
                "timestamp": "2026-01-15T12:00:00.000Z",
                "message": message,
            }))
            .unwrap()
        })
        .collect();
    entries.push(
        serde_json::from_value(json!({
            "type": "branch_summary",
            "id": "bs",
            "parentId": "e14",
            "timestamp": "2026-01-15T12:00:00.000Z",
            "fromId": "x",
            "summary": "Nested branch summary.",
        }))
        .unwrap(),
    );
    let mut failures = Vec::new();
    for case in selected(&fixture, &["branch_summary"]) {
        let input = &case.input;
        let (stream_fn, calls) = recorder("BRANCH");
        let options = GenerateBranchSummaryOptions {
            model: model(),
            api_key: Some("key".into()),
            headers: None,
            env: None,
            signal: CancellationToken::new(),
            custom_instructions: input["customInstructions"].as_str().map(str::to_string),
            replace_instructions: input["replaceInstructions"].as_bool(),
            reserve_tokens: None,
            stream_fn: Some(stream_fn),
            retry: None,
            callbacks: None,
        };
        let empty = input["empty"].as_bool().unwrap_or(false);
        let result = generate_branch_summary(if empty { &[] } else { &entries }, &options).await.unwrap();
        let actual = json!({ "calls": *calls.lock(), "result": without_usage(serde_json::to_value(&result).unwrap()) });
        check_json(&mut failures, &case.name, &case.out, &actual);
    }
    finish(failures);
}

#[tokio::test]
async fn golden_export_html() {
    use pi_coding_agent::core::export_html::{ExportOptions, ExportSessionToHtmlOptions, export_from_file};
    use sha2::{Digest, Sha256};
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in selected(&fixture, &["export_html"]) {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("session.jsonl");
        let lines: Vec<Value> = serde_json::from_value(case.input["lines"].clone()).unwrap();
        let text: String = lines.iter().map(|l| format!("{}\n", serde_json::to_string(l).unwrap())).collect();
        std::fs::write(&input, text).unwrap();
        let output = dir.path().join("out.html");
        let written = export_from_file(
            input.to_str().unwrap(),
            Some(ExportSessionToHtmlOptions::ExportOptions(ExportOptions {
                output_path: Some(output.to_string_lossy().into_owned()),
                theme_name: case.input["themeName"].as_str().map(str::to_string),
                tool_renderer: None,
            })),
        )
        .await
        .unwrap();
        let html = std::fs::read(written).unwrap();
        let actual = json!({ "sha256": format!("{:x}", Sha256::digest(&html)), "length": html.len() });
        check_json(&mut failures, &case.name, &case.out, &actual);
    }
    finish(failures);
}
