//! Example extensions with npm dependencies (from the TS reference checkout): each example is
//! copied to a temp directory, its dependencies are installed with pi's built-in (pure-Rust) npm
//! client, and it is loaded through the real loader and exercised. Skipped when the checkout is
//! missing or the npm registry is unreachable.

mod extensions_support;
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use extensions_support::*;
use pi_coding_agent::core::extensions::loader::load_extensions;
use pi_coding_agent::core::extensions::runner::ExtensionRunner;
use pi_coding_agent::core::package_manager::install_dependencies_with_builtin_npm;
use pi_coding_agent::core::resource_loader::{DefaultResourceLoader, DefaultResourceLoaderOptions, ResourceLoader};
use serde_json::{Value, json};

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            if entry.file_name() != "node_modules" {
                copy_dir(&path, &target);
            }
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

/// Copy an example directory and install its npm dependencies. `None` skips the test.
async fn prepare(example: &str) -> Option<PathBuf> {
    let Some(examples) = examples_dir() else {
        eprintln!("skipping: reference TypeScript sources not found (set PI_TS_SOURCE)");
        return None;
    };
    let dir = std::env::temp_dir().join(format!("pi-npm-example-{}", uuid::Uuid::new_v4())).join(example);
    copy_dir(&examples.join(example), &dir);
    if let Err(error) = install_dependencies_with_builtin_npm(&dir).await {
        eprintln!("skipping {example}: npm install failed (offline?): {error:#}");
        return None;
    }
    Some(dir)
}

async fn load(dir: &Path, entry: &str) -> Arc<ExtensionRunner> {
    let cwd = dir.display().to_string();
    let path = dir.join(entry).display().to_string();
    let result = load_extensions(&[path], &cwd, None, None).await.unwrap();
    assert!(result.errors.is_empty(), "{:#?}", result.errors);
    ExtensionRunner::new(&result.extensions, result.runtime.clone(), &cwd, session_manager(&cwd), model_registry().await)
}

async fn execute(runner: &ExtensionRunner, tool: &str, args: Value) -> anyhow::Result<pi_agent::types::AgentToolResult> {
    let definition = runner.get_tool_definition(tool).unwrap_or_else(|| panic!("tool {tool} is not registered"));
    (definition.execute)("call-1".into(), args, None, None, None).await
}

fn text_of(result: &pi_agent::types::AgentToolResult) -> String {
    serde_json::to_value(&result.content).unwrap()[0]["text"].as_str().unwrap_or_default().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_deps_resolves_its_own_node_modules() {
    let Some(dir) = prepare("with-deps").await else { return };
    assert!(dir.join("node_modules/ms/package.json").is_file());
    let runner = load(&dir, "index.ts").await;
    let result = execute(&runner, "parse_duration", json!({ "duration": "2 days" })).await.unwrap();
    assert_eq!(text_of(&result), "2 days = 172800000 milliseconds");
    let error = execute(&runner, "parse_duration", json!({ "duration": "soon" })).await.unwrap_err();
    assert!(error.to_string().contains("Invalid duration: \"soon\""), "{error:#}");
}

/// `custom-provider-anthropic`: the example's own Anthropic SDK client (`@anthropic-ai/sdk` from its
/// `node_modules`) streams a reply through pi's model runtime. The example's base URL is pointed at
/// a local mock of the Messages API.
///
/// Regression test for the vendored boa_ast scope-analyzer fix (`vendor/boa_ast/PATCH.md`): the SDK's
/// `MessageStream` creates functions inside computed class method keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custom_provider_anthropic_streams_with_the_sdk() {
    let Some(dir) = prepare("custom-provider-anthropic").await else { return };
    assert!(dir.join("node_modules/@anthropic-ai/sdk/package.json").is_file());
    let (base_url, requests) = mock_anthropic_server("hello from the sdk").await;
    let index = dir.join("index.ts");
    let source = std::fs::read_to_string(&index).unwrap();
    assert!(source.contains(r#"baseUrl: "https://api.anthropic.com""#));
    std::fs::write(&index, source.replace(r#"baseUrl: "https://api.anthropic.com""#, &format!("baseUrl: {base_url:?}"))).unwrap();
    // SAFETY: the variable is only read by this test's provider.
    unsafe { std::env::set_var("CUSTOM_ANTHROPIC_API_KEY", "sk-ant-api-test") };

    let cwd = dir.display().to_string();
    let loader = Arc::new(DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        cwd: cwd.clone(),
        agent_dir: cwd.clone(),
        additional_extension_paths: Some(vec![index.display().to_string()]),
        no_skills: Some(true),
        no_prompt_templates: Some(true),
        no_context_files: Some(true),
        ..Default::default()
    }));
    loader.reload(None).await.unwrap();
    assert!(loader.get_extensions().errors.is_empty(), "{:#?}", loader.get_extensions().errors);
    let harness = support::create_harness(support::HarnessOptions { resource_loader: Some(loader), ..Default::default() }).await.unwrap();
    let runtime = &harness.model_runtime;
    let model = runtime.get_model("custom-anthropic", "claude-sonnet-4-5").expect("the example registers its models");
    let context = pi_ai::types::Context {
        system_prompt: Some("Be brief.".into()),
        messages: vec![pi_ai::types::Message::User(pi_ai::types::UserMessage {
            content: pi_ai::types::UserContent::Text("hi".into()),
            timestamp: 0,
            ..Default::default()
        })],
        tools: None,
    };
    let message = runtime.complete_simple(model, context, None).await;
    assert_eq!(message.stop_reason, pi_ai::types::StopReason::Stop, "{:?}", message.error_message);
    let text: String = message.content.iter().filter_map(|c| c.as_text().map(|t| t.text.clone())).collect();
    assert_eq!(text, "hello from the sdk");
    let request = requests.lock().first().cloned().expect("the SDK called the mock server");
    assert!(request.starts_with("POST /v1/messages"), "{request}");
    assert!(request.to_ascii_lowercase().contains("x-api-key: sk-ant-api-test"), "{request}");
    harness.cleanup();
}

/// A mock HTTP server routing by path. Records `"<request line>\n<headers>\n\n<body>"` per request.
async fn mock_router(route: fn(&str, &str) -> (u16, &'static str, String)) -> (String, Arc<parking_lot::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let requests: Arc<parking_lot::Mutex<Vec<String>>> = Arc::default();
    let seen = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let text = loop {
                    let Ok(n) = socket.read(&mut chunk).await else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + length {
                            break text;
                        }
                    }
                };
                seen.lock().push(text.clone());
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
                let (status, content_type, reply) = route(&path, &body);
                let response = format!(
                    "HTTP/1.1 {status} OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (base_url, requests)
}

fn gitlab_route(path: &str, _body: &str) -> (u16, &'static str, String) {
    let path = path.split('?').next().unwrap_or(path);
    let sse = |events: Vec<Value>, named: bool| -> String {
        events
            .iter()
            .map(|e| match named {
                true => format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()),
                false => format!("data: {e}\n\n"),
            })
            .collect()
    };
    if path == "/api/v4/ai/third_party_agents/direct_access" {
        return (200, "application/json", json!({ "token": "direct-token", "headers": { "x-gitlab-instance-id": "instance-1" } }).to_string());
    }
    if path.starts_with("/ai/v1/proxy/anthropic") && path.ends_with("/v1/messages") {
        let events = vec![
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"mock","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"duo claude"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ];
        return (200, "text/event-stream", sse(events, true));
    }
    if path == "/ai/v1/proxy/openai/v1/responses" {
        let events = vec![
            json!({ "type": "response.output_item.added", "item": { "type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": [] } }),
            json!({ "type": "response.content_part.added", "part": { "type": "output_text", "text": "" } }),
            json!({ "type": "response.output_text.delta", "delta": "duo gpt" }),
            json!({ "type": "response.output_item.done", "item": { "type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "content": [{ "type": "output_text", "text": "duo gpt" }] } }),
            json!({ "type": "response.completed", "response": { "status": "completed", "usage": { "input_tokens": 5, "output_tokens": 3, "total_tokens": 8, "input_tokens_details": { "cached_tokens": 0 } } } }),
        ];
        return (200, "text/event-stream", sse(events, false));
    }
    (404, "text/plain", format!("no route for {path}"))
}

/// `custom-provider-gitlab-duo` (no npm dependencies): the provider exchanges the GitLab token for a
/// direct-access token, then streams through pi-ai's Anthropic Messages and OpenAI Responses
/// implementations. GitLab and the AI gateway are pointed at a local mock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custom_provider_gitlab_duo_streams_through_both_backends() {
    let Some(dir) = prepare("custom-provider-gitlab-duo").await else { return };
    let (base_url, requests) = mock_router(gitlab_route).await;
    let index = dir.join("index.ts");
    let source = std::fs::read_to_string(&index).unwrap();
    for constant in [r#"const GITLAB_COM_URL = "https://gitlab.com";"#, r#"const AI_GATEWAY_URL = "https://cloud.gitlab.com";"#] {
        assert!(source.contains(constant), "{constant}");
    }
    let source = source
        .replace(r#"const GITLAB_COM_URL = "https://gitlab.com";"#, &format!("const GITLAB_COM_URL = {base_url:?};"))
        .replace(r#"const AI_GATEWAY_URL = "https://cloud.gitlab.com";"#, &format!("const AI_GATEWAY_URL = {base_url:?};"));
    std::fs::write(&index, source).unwrap();
    // SAFETY: only this test's provider reads GITLAB_TOKEN.
    unsafe { std::env::set_var("GITLAB_TOKEN", "glpat-test") };

    let cwd = dir.display().to_string();
    let loader = Arc::new(DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        cwd: cwd.clone(),
        agent_dir: cwd.clone(),
        additional_extension_paths: Some(vec![dir.display().to_string()]),
        no_skills: Some(true),
        no_prompt_templates: Some(true),
        no_context_files: Some(true),
        ..Default::default()
    }));
    loader.reload(None).await.unwrap();
    assert!(loader.get_extensions().errors.is_empty(), "{:#?}", loader.get_extensions().errors);
    let harness = support::create_harness(support::HarnessOptions { resource_loader: Some(loader), ..Default::default() }).await.unwrap();
    harness.session.bind_extensions(&Default::default()).await.unwrap();
    let runtime = harness.model_runtime.clone();
    let context = || pi_ai::types::Context {
        system_prompt: Some("Be brief.".into()),
        messages: vec![pi_ai::types::Message::User(pi_ai::types::UserMessage {
            content: pi_ai::types::UserContent::Text("hi".into()),
            timestamp: 0,
            ..Default::default()
        })],
        tools: None,
    };
    let text = |message: &pi_ai::types::AssistantMessage| -> String {
        message.content.iter().filter_map(|c| c.as_text().map(|t| t.text.clone())).collect()
    };

    let claude = runtime.get_model("gitlab-duo", "claude-sonnet-4-6").expect("the example registers its models");
    let message = runtime.complete_simple(claude, context(), None).await;
    assert_eq!(message.stop_reason, pi_ai::types::StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(text(&message), "duo claude");

    let gpt = runtime.get_model("gitlab-duo", "gpt-5-codex").unwrap();
    let message = runtime.complete_simple(gpt, context(), None).await;
    assert_eq!(message.stop_reason, pi_ai::types::StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(text(&message), "duo gpt");

    let requests = requests.lock().clone();
    let lower: Vec<String> = requests.iter().map(|r| r.to_ascii_lowercase()).collect();
    // One direct-access exchange (cached for both requests) with the GitLab token.
    let exchanges: Vec<&String> = lower.iter().filter(|r| r.starts_with("post /api/v4/ai/third_party_agents/direct_access")).collect();
    assert_eq!(exchanges.len(), 1, "{requests:#?}");
    assert!(exchanges[0].contains("authorization: bearer glpat-test"), "{}", exchanges[0]);
    let anthropic = lower.iter().find(|r| r.contains("/ai/v1/proxy/anthropic")).expect("anthropic request");
    assert!(anthropic.contains("authorization: bearer direct-token") && anthropic.contains("x-gitlab-instance-id: instance-1"), "{anthropic}");
    let openai = lower.iter().find(|r| r.starts_with("post /ai/v1/proxy/openai/v1/responses")).expect("openai request");
    assert!(openai.contains("authorization: bearer direct-token") && openai.contains("x-gitlab-instance-id: instance-1"), "{openai}");
    harness.cleanup();
}

/// Start a runner with a recording UI and emit `session_start`.
async fn start_session(runner: &Arc<ExtensionRunner>) -> Arc<TestUi> {
    use pi_coding_agent::core::extensions::types::{ExtensionEvent, ExtensionMode, SessionStartEvent, SessionStartReason};
    let ui = Arc::new(TestUi::default());
    runner.set_ui_context(Some(ui.clone()), Some(ExtensionMode::Tui));
    runner
        .emit(ExtensionEvent::SessionStart(SessionStartEvent { reason: SessionStartReason::Startup, previous_session_file: None }))
        .await
        .unwrap();
    ui
}

fn on_path(binary: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
}

/// `sandbox` (`@anthropic-ai/sandbox-runtime`): `SandboxManager.initialize` starts its HTTP and SOCKS
/// filtering proxies on pi-jsrt's `node:net`/`node:http`, then the example's `bash` tool runs commands
/// in bubblewrap (writes only to the allowed paths). Needs bwrap, socat, rg and curl (skipped
/// otherwise). Platform limits seen identically under Node in this container: bubblewrap's private
/// network namespace has no IPv6, so the in-sandbox `socat TCP-LISTEN` bridge to the proxies cannot
/// listen (commands get "Failed to connect to localhost port 3128"), and bubblewrap leaves its mount
/// points for denied paths as empty files in the writable working directory, after which
/// sandbox-runtime 0.0.26 cannot build the next sandbox ("Can't mkdir parents for .claude/commands").
/// So everything is checked in one command, and the proxies are exercised from the host side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sandbox_runs_bash_in_bubblewrap_behind_its_filtering_proxies() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if !["bwrap", "socat", "rg", "curl"].iter().all(|b| on_path(b)) {
        eprintln!("skipping: sandbox needs bwrap, socat, rg and curl");
        return;
    }
    let Some(dir) = prepare("sandbox").await else { return };
    let runner = load(&dir, "index.ts").await;
    let ui = start_session(&runner).await;
    assert_eq!(*ui.notifications.lock(), vec!["Sandbox initialized".to_string()]);
    let command = [
        "touch ./inside && echo ok-inside",
        "(touch /etc/pi-sandbox-probe 2>/dev/null && echo etc-writable) || echo etc-denied",
        "echo proxy=$HTTPS_PROXY",
        "echo ports=$CLAUDE_CODE_HOST_HTTP_PROXY_PORT,$CLAUDE_CODE_HOST_SOCKS_PROXY_PORT",
    ]
    .join("; ");
    let output = text_of(&execute(&runner, "bash", json!({ "command": command })).await.unwrap());
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines[..3], ["ok-inside", "etc-denied", "proxy=http://localhost:3128"], "{output}");
    assert!(dir.join("inside").is_file());
    assert!(!Path::new("/etc/pi-sandbox-probe").exists());
    let ports: Vec<u16> = lines[3].trim_start_matches("ports=").split(',').map(|p| p.parse().unwrap()).collect();

    // The HTTP proxy (pi-jsrt node:http server) refuses a CONNECT to a host outside the allowlist.
    let mut http = tokio::net::TcpStream::connect(("127.0.0.1", ports[0])).await.unwrap();
    http.write_all(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n").await.unwrap();
    let mut reply = String::new();
    http.read_to_string(&mut reply).await.unwrap();
    assert!(reply.starts_with("HTTP/1.1 403 Forbidden") && reply.contains("X-Proxy-Error: blocked-by-allowlist"), "{reply}");

    // The SOCKS5 proxy (node:net server) negotiates and refuses the same destination.
    let mut socks = tokio::net::TcpStream::connect(("127.0.0.1", ports[1])).await.unwrap();
    socks.write_all(&[5, 1, 0]).await.unwrap();
    let mut greeting = [0u8; 2];
    socks.read_exact(&mut greeting).await.unwrap();
    assert_eq!(greeting, [5, 0]);
    let mut request = vec![5, 1, 0, 3, b"example.com".len() as u8];
    request.extend_from_slice(b"example.com");
    request.extend_from_slice(&443u16.to_be_bytes());
    socks.write_all(&request).await.unwrap();
    let mut response = [0u8; 2];
    socks.read_exact(&mut response).await.unwrap();
    assert_eq!(response[0], 5);
    assert_ne!(response[1], 0, "the SOCKS proxy must not connect to a host outside the allowlist");
}
