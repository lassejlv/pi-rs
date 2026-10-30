//! `ctx.modelRegistry` and `ctx.sessionManager` of JavaScript extensions behave like the TS
//! objects extensions receive: every public `ModelRegistry` / `SessionManager` method, the same
//! return shapes (undefined vs null), and the same object identities (`getProvider()`,
//! `getRegisteredProviderConfig()` and `getRegisteredNativeProvider()` return the same object
//! until a registration changes it, the extension's own config functions and provider objects
//! come back unchanged). The expected values were recorded with the TS pi.

mod extensions_support;
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use pi_coding_agent::core::resource_loader::{DefaultResourceLoader, DefaultResourceLoaderOptions, ResourceLoader};
use serde_json::{Value, json};
use support::*;

/// Load `source` (which writes its findings to `globalThis.RESULT_PATH`) into a fresh session and
/// return what it wrote once `session_start` ran.
async fn run_probe(source: &str) -> (Value, Harness, PathBuf) {
    run_probe_with(source, |_| {}).await
}

/// [`run_probe`] with a hook to prepare the harness (faux responses) before extensions bind.
async fn run_probe_with(source: &str, prepare: impl FnOnce(&Harness)) -> (Value, Harness, PathBuf) {
    let dir = std::env::temp_dir().join(format!("pi-ext-ctx-api-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let result = dir.join("result.json");
    let path = dir.join("extension.ts");
    let source = format!("const RESULT_PATH = {};\n{source}", serde_json::to_string(&result.display().to_string()).unwrap());
    std::fs::write(&path, source).unwrap();
    let loader = Arc::new(DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        cwd: dir.display().to_string(),
        agent_dir: dir.display().to_string(),
        additional_extension_paths: Some(vec![path.display().to_string()]),
        no_skills: Some(true),
        no_prompt_templates: Some(true),
        no_context_files: Some(true),
        ..Default::default()
    }));
    loader.reload(None).await.unwrap();
    assert!(loader.get_extensions().errors.is_empty(), "{:#?}", loader.get_extensions().errors);
    let harness = create_harness(HarnessOptions { resource_loader: Some(loader), ..Default::default() }).await.unwrap();
    prepare(&harness);
    harness.session.bind_extensions(&Default::default()).await.unwrap();
    let done = dir.join("result.json.done");
    for _ in 0..600 {
        if done.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let text = std::fs::read_to_string(&result).unwrap_or_else(|_| "null".into());
    (serde_json::from_str(&text).unwrap(), harness, dir)
}

const PROBE_PRELUDE: &str = r#"
import { writeFileSync } from "node:fs";
const out = {};
const t = async (k, f) => {
    try { out[k] = await f(); } catch (e) { out[k] = "THROW " + e.message; }
    writeFileSync(RESULT_PATH, JSON.stringify(out));
};
const finish = () => {
    writeFileSync(RESULT_PATH, JSON.stringify(out));
    writeFileSync(RESULT_PATH + ".done", "");
};
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_registry_matches_ts() {
    let source = format!(
        "{PROBE_PRELUDE}{}",
        r#"
export default function (pi) {
    pi.on("session_start", async (_event, ctx) => {
        const r = ctx.modelRegistry;
        await t("sameRegistry", () => r === ctx.modelRegistry);
        await t("methods", () => Object.getOwnPropertyNames(Object.getPrototypeOf(r)).sort());
        await t("unknownProp", () => typeof r.notAThing);
        const p1 = r.getProvider("anthropic");
        await t("provider.same", () => p1 === r.getProvider("anthropic"));
        await t("provider.keys", () => Object.keys(p1));
        await t("provider.missing", () => r.getProvider("nope") === undefined);
        await t("cfg.none", () => r.getRegisteredProviderConfig("anthropic") === undefined);
        await t("native.none", () => r.getRegisteredNativeProvider("anthropic") === undefined);
        const streamSimple = () => { throw new Error("x"); };
        r.registerProvider("anthropic", { api: "anthropic-messages", streamSimple, headers: { a: "b" } });
        const c1 = r.getRegisteredProviderConfig("anthropic");
        await t("cfg1", () => ({ keys: Object.keys(c1).sort(), same: c1 === r.getRegisteredProviderConfig("anthropic"), fn: c1.streamSimple === streamSimple, headers: c1.headers }));
        await t("provider.changed", () => r.getProvider("anthropic") !== p1);
        const p2 = r.getProvider("anthropic");
        await t("provider.stable", () => p2 === r.getProvider("anthropic"));
        r.registerProvider("anthropic", { name: "Renamed" });
        const c2 = r.getRegisteredProviderConfig("anthropic");
        await t("cfg2", () => ({ keys: Object.keys(c2).sort(), newObject: c2 !== c1, fn: c2.streamSimple === streamSimple, name: c2.name }));
        await t("ids.registered", () => r.getRegisteredProviderIds().includes("anthropic"));
        await t("displayName", () => r.getProviderDisplayName("anthropic"));
        r.unregisterProvider("anthropic");
        await t("cfg.after", () => r.getRegisteredProviderConfig("anthropic") === undefined);
        await t("ids.unregistered", () => r.getRegisteredProviderIds().includes("anthropic"));
        const native = { id: "probe-native", name: "Probe Native", auth: {}, getModels: () => [], stream: () => { throw new Error("no"); }, streamSimple: () => { throw new Error("no"); } };
        r.registerProvider(native);
        await t("native.same", () => r.getRegisteredNativeProvider("probe-native") === native);
        await t("native.effective", () => r.getProvider("probe-native") === native);
        r.registerProvider(p1);
        await t("builtin.reregistered", () => ({ native: r.getRegisteredNativeProvider("anthropic") === p1, effective: r.getProvider("anthropic") === p1 }));
        r.unregisterProvider("anthropic");
        r.unregisterProvider("probe-native");
        await t("registerWithoutConfig", () => r.registerProvider("x"));
        const model = r.getAll()[0];
        r.registerVirtualModel({ provider: model.provider, id: "virt", name: "Virtual", route: () => ({ model }) });
        await t("virtual", () => { const m = r.find(model.provider, "virt"); return m && { id: m.id, name: m.name }; });
        r.unregisterVirtualModel(model.provider, "virt");
        await t("virtual.after", () => r.find(model.provider, "virt") === undefined);
        await t("refresh", async () => { const res = await r.refresh({ allowNetwork: false }); return { aborted: res.aborted, isMap: res.errors instanceof Map, size: res.errors.size }; });
        await t("find.none", () => r.find("nope", "zzz") === undefined);
        await t("auth.none", async () => (await r.getProviderAuth("nope")) === undefined);
        finish();
    });
}
"#
    );
    let (result, harness, dir) = run_probe(&source).await;
    let expected = json!({
        "sameRegistry": true,
        "methods": [
            "classify", "complete", "constructor", "find", "findOfType", "generateImages", "getAll",
            "getApiKeyAndHeaders", "getApiKeyForProvider", "getAvailable", "getAvailableOfType", "getError",
            "getModelOfType", "getModelsOfType", "getProvider", "getProviderAuth", "getProviderAuthStatus",
            "getProviderDisplayName", "getRegisteredNativeProvider", "getRegisteredProviderConfig",
            "getRegisteredProviderIds", "hasConfiguredAuth", "isUsingOAuth", "refresh", "registerProvider",
            "registerVirtualModel", "stream", "streamSimple", "unregisterProvider", "unregisterVirtualModel"
        ],
        "unknownProp": "undefined",
        "provider.same": true,
        "provider.keys": [
            "id", "name", "baseUrl", "headers", "auth", "getModels", "getAllModels", "refreshModels",
            "filterModels", "filterAllModels", "stream", "streamSimple"
        ],
        "provider.missing": true,
        "cfg.none": true,
        "native.none": true,
        "cfg1": { "keys": ["api", "headers", "streamSimple"], "same": true, "fn": true, "headers": { "a": "b" } },
        "provider.changed": true,
        "provider.stable": true,
        "cfg2": { "keys": ["api", "headers", "name", "streamSimple"], "newObject": true, "fn": true, "name": "Renamed" },
        "ids.registered": true,
        "displayName": "Renamed",
        "cfg.after": true,
        "ids.unregistered": false,
        "native.same": true,
        "native.effective": true,
        "builtin.reregistered": { "native": true, "effective": true },
        "registerWithoutConfig": "THROW Provider config is required when registering by name",
        "virtual": { "id": "virt", "name": "Virtual" },
        "virtual.after": true,
        "refresh": { "aborted": false, "isMap": true, "size": 0 },
        "find.none": true,
        "auth.none": true,
    });
    assert_eq!(result, expected);
    harness.cleanup();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_manager_matches_ts() {
    let source = format!(
        "{PROBE_PRELUDE}{}",
        r#"
import { mkdtempSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { SessionManager } from "@earendil-works/pi-coding-agent";
const usage = { input: 1, output: 2, cacheRead: 0, cacheWrite: 0, totalTokens: 3, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };
const assistant = (text, timestamp) => ({ role: "assistant", content: [{ type: "text", text }], api: "x", provider: "p", model: "m", usage, stopReason: "stop", timestamp });
export default function (pi) {
    pi.on("session_start", async (_event, ctx) => {
        const sm = ctx.sessionManager;
        await t("same", () => sm === ctx.sessionManager);
        await t("instanceof", () => sm instanceof SessionManager);
        await t("header.type", () => sm.getHeader().type);
        await t("label.missing", () => sm.getLabel("zz") === undefined);
        await t("unknownProp", () => typeof sm.notAThing);
        const m = SessionManager.inMemory("/tmp");
        await t("mem.leaf", () => m.getLeafId());
        const id1 = m.appendMessage({ role: "user", content: "hello", timestamp: 1 });
        const id2 = m.appendMessage(assistant("hi", 2));
        m.appendThinkingLevelChange("high");
        m.appendModelChange("p", "m");
        m.appendCustomEntry("x", { a: 1 });
        m.appendSessionInfo("My Session");
        await t("mem.name", () => m.getSessionName());
        m.appendCustomMessageEntry("ct", "text", true, { d: 1 });
        await t("mem.label", () => { m.appendLabelChange(id1, "L1"); return m.getLabel(id1); });
        await t("mem.editBad", () => m.appendContextEdit(id1, { nope: 1 }));
        m.appendContextEdit(id1, { content: "edited" });
        m.appendCompaction("sum", id2, 123, { x: 1 }, true);
        await t("mem.usage", () => { const e = m.appendUsage("compaction", "p", "m", usage, "note"); return [e.type, e.kind, e.note, e.usage.totalTokens]; });
        await t("mem.types", () => m.getEntries().map((e) => e.type));
        await t("mem.context", () => m.buildSessionContext().messages.map((msg) => msg.role));
        await t("mem.branch", () => { m.branch(id1); return m.getLeafId() === id1; });
        await t("mem.summary", () => { m.branchWithSummary(id1, "summary", { d: 2 }, false); return m.getLeafEntry().type; });
        await t("mem.reset", () => { m.resetLeaf(); return m.getLeafId(); });
        await t("mem.branchMissing", () => m.branch("nope"));
        const dir = mkdtempSync(join(tmpdir(), "sm-probe-"));
        const c = SessionManager.create("/tmp/sm-probe-cwd", dir);
        c.appendMessage({ role: "user", content: "first message", timestamp: 1 });
        c.appendMessage(assistant("ok", 2));
        await t("create.persisted", () => [typeof c.getSessionFile(), c.isPersisted()]);
        const list = await SessionManager.list("/tmp/sm-probe-cwd", dir);
        await t("list", () => list.map((s) => [s.firstMessage, s.messageCount, s.created instanceof Date, s.modified instanceof Date]));
        const opened = SessionManager.open(c.getSessionFile(), dir);
        await t("open.count", () => opened.getEntryCount());
        await t("findById", () => SessionManager.findById("/tmp/sm-probe-cwd", c.getSessionId(), dir) === c.getSessionFile());
        await t("listAll", async () => (await SessionManager.listAll(dir)).length);
        await t("constructor", () => { const x = new SessionManager("/tmp", "", undefined, false); return [x.getCwd(), x.isPersisted(), x.getLeafId()]; });
        finish();
    });
}
"#
    );
    let (result, harness, dir) = run_probe(&source).await;
    let expected = json!({
        "same": true,
        "instanceof": true,
        "header.type": "session",
        "label.missing": true,
        "unknownProp": "undefined",
        "mem.leaf": null,
        "mem.name": "My Session",
        "mem.label": "L1",
        "mem.editBad": "THROW Context edit replacement must be null or contain string/array content",
        "mem.usage": ["usage", "compaction", "note", 3],
        "mem.types": [
            "message", "message", "thinking_level_change", "model_change", "custom", "session_info",
            "custom_message", "label", "context_edit", "compaction", "usage"
        ],
        "mem.context": ["compactionSummary", "assistant", "custom"],
        "mem.branch": true,
        "mem.summary": "branch_summary",
        "mem.reset": null,
        "mem.branchMissing": "THROW Entry nope not found",
        "create.persisted": ["string", true],
        "list": [["first message", 2, true, true]],
        "open.count": 2,
        "findById": true,
        "listAll": 1,
        "constructor": ["/tmp", false, null],
    });
    assert_eq!(result, expected);
    harness.cleanup();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_manager_matches_ts() {
    let source = format!(
        "{PROBE_PRELUDE}{}",
        r#"
import { mkdtempSync, mkdirSync, writeFileSync as write, readFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { SettingsManager } from "@earendil-works/pi-coding-agent";
export default function (pi) {
    pi.on("session_start", async () => {
        const root = mkdtempSync(join(tmpdir(), "settings-probe-"));
        const agentDir = join(root, "agent");
        const cwd = join(root, "project");
        mkdirSync(agentDir, { recursive: true });
        mkdirSync(join(cwd, ".pi"), { recursive: true });
        write(join(agentDir, "settings.json"), JSON.stringify({ theme: "light", defaultProvider: "anthropic", compaction: { reserveTokens: 1000 }, packages: ["npm:foo"] }));
        write(join(cwd, ".pi", "settings.json"), JSON.stringify({ defaultModel: "claude-x", hideThinkingBlock: true }));
        const sm = SettingsManager.create(cwd, agentDir);
        await t("ctor", () => { try { new SettingsManager(); return "no throw"; } catch { return "throws"; } });
        await t("theme", () => sm.getTheme());
        await t("provider", () => [sm.getDefaultProvider(), sm.getDefaultModel()]);
        await t("hide", () => sm.getHideThinkingBlock());
        await t("compaction", () => [sm.getCompactionReserveTokens(), sm.getCompactionKeepRecentTokens(), sm.getCompactionEnabled()]);
        await t("packages", () => sm.getPackages());
        await t("global", () => sm.getGlobalSettings());
        await t("project", () => sm.getProjectSettings());
        await t("steering", () => [sm.getSteeringMode(), sm.getFollowUpMode(), sm.getTransport(), sm.getDoubleEscapeAction(), sm.getTreeFilterMode()]);
        await t("missing", () => [sm.getShellPath(), sm.getSessionDir(), sm.getThinkingBudgets(), sm.getEnabledModels()]);
        await t("retry", () => sm.getRetrySettings());
        await t("overrides", () => sm.getTerminalCapabilityOverrides());
        sm.setTheme("dark");
        sm.setDefaultModelAndProvider("openai", "gpt-x");
        sm.setModelThinkingLevel("openai", "gpt-x", "high");
        await t("thinking", () => [sm.getModelThinkingLevel("openai", "gpt-x"), sm.getAllModelThinkingLevels()]);
        await sm.flush();
        await t("file", () => JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf8")));
        await t("errors", () => sm.drainErrors());
        const mem = SettingsManager.inMemory({ theme: "x", shellPath: "/bin/zsh" });
        await t("mem", () => [mem.getTheme(), mem.getShellPath(), mem.getQuietStartup()]);
        let stored = { global: JSON.stringify({ theme: "stored" }), project: undefined };
        const storage = { withLock(scope, fn) { const next = fn(stored[scope]); if (next !== undefined) stored[scope] = next; } };
        const fromStorage = SettingsManager.fromStorage(storage);
        await t("storage.theme", () => fromStorage.getTheme());
        fromStorage.setTheme("written");
        await fromStorage.flush();
        await t("storage.written", () => JSON.parse(stored.global).theme);
        finish();
    });
}
"#
    );
    let (result, harness, dir) = run_probe(&source).await;
    let expected: Value = serde_json::from_str(include_str!("fixtures/extensions_settings_manager_ts.json")).unwrap();
    assert_eq!(result, expected);
    harness.cleanup();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_runtime_matches_ts() {
    let source = format!(
        "{PROBE_PRELUDE}{}",
        r#"
import { mkdtempSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { ModelRegistry, ModelRuntime } from "@earendil-works/pi-coding-agent";
export default function (pi) {
    pi.on("session_start", async (_event, ctx) => {
        const runtime = ctx.modelRegistry.runtime;
        await t("same", () => runtime === ctx.modelRegistry.runtime);
        await t("instanceof", () => runtime instanceof ModelRuntime);
        await t("provider", () => runtime.getProvider("anthropic") === ctx.modelRegistry.getProvider("anthropic"));
        await t("providers", () => runtime.getProviders().some((p) => p.id === "anthropic"));
        await t("models", () => runtime.getModels("anthropic").length === ctx.modelRegistry.getAll().filter((m) => m.provider === "anthropic").length);
        await t("model", () => { const m = runtime.getModels("anthropic")[0]; return runtime.getModel("anthropic", m.id).id === m.id; });
        await t("auth", () => [runtime.hasConfiguredAuth("nope"), runtime.isUsingOAuth("nope"), runtime.isUsingSubscription("nope"), runtime.getProviderAuthStatus("nope")]);
        await t("checkAuth", async () => (await runtime.checkAuth("nope")) === undefined);
        await t("getAuth", async () => (await runtime.getAuth("nope")) === undefined);
        await t("ctor", () => { try { new ModelRuntime(); return "no throw"; } catch { return "throws"; } });
        const root = mkdtempSync(join(tmpdir(), "runtime-probe-"));
        const created = await ModelRuntime.create({ authPath: join(root, "auth.json"), modelsPath: null, refreshOnCreate: false });
        await t("created", () => [created instanceof ModelRuntime, created.getModels("openai").length > 0, created.getRegisteredProviderIds()]);
        await created.setRuntimeApiKey("openai", "sk-test");
        await t("runtimeKey", async () => [(await created.getAuth("openai"))?.auth?.apiKey, created.hasConfiguredAuth("openai")]);
        const registry = new ModelRegistry(created);
        await t("registry", () => [registry instanceof ModelRegistry, registry.runtime === created, registry.find("openai", created.getModels("openai")[0].id)?.provider]);
        created.registerProvider("probe-provider", { baseUrl: "http://localhost:1/v1", apiKey: "k", api: "openai-completions", models: [{ id: "m1", name: "M1", reasoning: false, input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 1000, maxTokens: 100 }] });
        await t("registered", () => [created.getModel("probe-provider", "m1")?.name, registry.getRegisteredProviderIds(), Object.keys(created.getRegisteredProviderConfig("probe-provider")).sort()]);
        const refreshed = await created.refresh({ allowNetwork: false });
        await t("refresh", () => [refreshed.aborted, refreshed.errors instanceof Map]);
        finish();
    });
}
"#
    );
    let (result, harness, dir) = run_probe(&source).await;
    let expected: Value = serde_json::from_str(include_str!("fixtures/extensions_model_runtime_ts.json")).unwrap();
    assert_eq!(result, expected);
    harness.cleanup();
    let _ = std::fs::remove_dir_all(dir);
}

/// `createAgentSession()` with a `DefaultResourceLoader` (inline factories, a built-in factory,
/// prompt overrides), custom tools and the parent's model runtime runs a real child session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_agent_session_runs_a_child_session() {
    let source = format!(
        "{PROBE_PRELUDE}{}",
        r#"
import { mkdtempSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import {
    AgentSession, createAgentSession, createCodemodeExtension, DefaultResourceLoader, SessionManager, SettingsManager,
} from "@earendil-works/pi-coding-agent";
export default function (pi) {
    pi.on("session_start", async (_event, ctx) => {
        const agentDir = mkdtempSync(join(tmpdir(), "child-agent-"));
        const settingsManager = SettingsManager.inMemory({});
        const loader = new DefaultResourceLoader({
            cwd: ctx.cwd,
            agentDir,
            settingsManager,
            noSkills: true,
            noPromptTemplates: true,
            noThemes: true,
            noContextFiles: true,
            systemPromptOverride: () => "You are a child agent.",
            appendSystemPromptOverride: () => [],
            extensionFactories: [
                (childPi) => {
                    childPi.registerTool({
                        name: "child_tool",
                        label: "Child",
                        description: "A tool from an inline factory",
                        parameters: { type: "object", properties: {} },
                        async execute() { return { content: [{ type: "text", text: "ok" }], details: {} }; },
                    });
                    childPi.on("session_start", () => { globalThis.childSessionStarted = true; });
                },
                { name: "codemode", factory: createCodemodeExtension(), builtin: true, replaceable: true },
            ],
        });
        await loader.reload();
        await t("loader", () => loader.getExtensions().extensions.map((e) => [e.path, [...e.tools.keys()]]));
        await t("systemPrompt", () => loader.getSystemPrompt());
        const customTool = {
            name: "custom_one",
            label: "Custom",
            description: "A custom tool",
            parameters: { type: "object", properties: {} },
            async execute() { return { content: [{ type: "text", text: "custom" }], details: {} }; },
        };
        const { session } = await createAgentSession({
            cwd: ctx.cwd,
            agentDir,
            sessionManager: SessionManager.inMemory(ctx.cwd),
            settingsManager,
            modelRuntime: ctx.modelRegistry.runtime,
            model: ctx.model,
            resourceLoader: loader,
            tools: ["read", "child_tool", "custom_one"],
            customTools: [customTool],
        });
        await t("instanceof", () => session instanceof AgentSession);
        const errors = [];
        await session.bindExtensions({ onError: (e) => errors.push(e) });
        await t("childStarted", () => globalThis.childSessionStarted === true);
        await t("activeTools", () => session.getActiveToolNames().sort());
        await t("toolDefinition", () => session.getToolDefinition("custom_one") === customTool);
        await t("builtinDefinition", () => typeof session.getToolDefinition("read")?.execute);
        await t("systemPromptPrefix", () => session.systemPrompt.startsWith("You are a child agent."));
        const events = [];
        const unsubscribe = session.subscribe((event) => events.push(event.type));
        let hooked = 0;
        const prior = session.agent.beforeToolCall;
        session.agent.beforeToolCall = async (context, signal) => { hooked++; return prior?.(context, signal); };
        await t("hookRoundTrip", () => typeof session.agent.beforeToolCall);
        session.setSessionName("child#1");
        await session.prompt("hello child");
        unsubscribe();
        await t("messages", () => session.messages.map((m) => m.role));
        await t("lastText", () => session.getLastAssistantText());
        await t("events", () => ["agent_start", "turn_end", "agent_end"].every((type) => events.includes(type)));
        await t("sessionName", () => [session.sessionName, session.sessionManager.getSessionName()]);
        await t("stats", () => session.getSessionStats().userMessages);
        await t("errors", () => errors.length);
        await t("model", () => session.model?.id === ctx.model?.id);
        session.dispose();
        finish();
    });
}
"#
    );
    let (result, harness, dir) = run_probe_with(&source, |h| {
        h.set_responses(vec![faux_text_message("child reply").into()]);
    })
    .await;
    assert_eq!(result["loader"], json!([["builtin:codemode", ["codemode"]], ["<inline:1>", ["child_tool"]]]), "{result:#}");
    assert_eq!(result["systemPrompt"], json!("You are a child agent."), "{result:#}");
    assert_eq!(result["instanceof"], json!(true), "{result:#}");
    assert_eq!(result["childStarted"], json!(true), "{result:#}");
    assert_eq!(result["activeTools"], json!(["child_tool", "custom_one", "read"]), "{result:#}");
    assert_eq!(result["toolDefinition"], json!(true), "{result:#}");
    assert_eq!(result["builtinDefinition"], json!("function"), "{result:#}");
    assert_eq!(result["systemPromptPrefix"], json!(true), "{result:#}");
    assert_eq!(result["hookRoundTrip"], json!("function"), "{result:#}");
    assert_eq!(result["messages"], json!(["system", "user", "assistant"]), "{result:#}");
    assert_eq!(result["lastText"], json!("child reply"), "{result:#}");
    assert_eq!(result["events"], json!(true), "{result:#}");
    assert_eq!(result["sessionName"], json!(["child#1", "child#1"]), "{result:#}");
    assert_eq!(result["stats"], json!(1), "{result:#}");
    assert_eq!(result["errors"], json!(0), "{result:#}");
    assert_eq!(result["model"], json!(true), "{result:#}");
    harness.cleanup();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdk_helpers_match_ts() {
    let source = format!(
        "{PROBE_PRELUDE}{}",
        r#"
import { mkdtempSync, mkdirSync, writeFileSync as write } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import {
    createAgentSession, createExtensionRuntime, DefaultPackageManager, discoverAndLoadExtensions, ProjectTrustStore,
    resolveCliModel, resolveModelScopeWithDiagnostics, SessionManager, SettingsManager,
} from "@earendil-works/pi-coding-agent";
export default function (pi) {
    pi.on("session_start", async (_event, ctx) => {
        const root = mkdtempSync(join(tmpdir(), "sdk-probe-"));
        const agentDir = join(root, "agent");
        const cwd = join(root, "project");
        mkdirSync(agentDir, { recursive: true });
        mkdirSync(cwd, { recursive: true });
        const trust = new ProjectTrustStore(agentDir);
        await t("trust.initial", () => [trust.get(cwd) === undefined, trust.getEntry(cwd)]);
        trust.set(cwd, true);
        await t("trust.set", () => [trust.get(cwd), new ProjectTrustStore(agentDir).getEntry(cwd)?.decision]);
        trust.setMany([{ path: cwd, decision: false }]);
        await t("trust.many", () => trust.get(cwd));
        const settingsManager = SettingsManager.create(cwd, agentDir);
        const pm = new DefaultPackageManager({ cwd, agentDir, settingsManager });
        await t("pm.list", () => pm.listConfiguredPackages());
        await t("pm.add", () => [pm.addSourceToSettings("npm:some-package"), pm.listConfiguredPackages().map((p) => [p.source, p.scope, p.filtered])]);
        await t("pm.path", () => pm.getInstalledPath("npm:some-package", "user") === undefined);
        await t("pm.remove", () => [pm.removeSourceFromSettings("npm:some-package"), pm.listConfiguredPackages().length]);
        await t("pm.resolve", async () => { const r = await pm.resolve(); return Object.keys(r).sort(); });
        const runtime = ctx.modelRegistry.runtime;
        const model = ctx.model;
        await t("cli.none", () => resolveCliModel({ modelRuntime: runtime }));
        await t("cli.match", () => { const r = resolveCliModel({ cliModel: `${model.provider}/${model.id}`, modelRuntime: runtime }); return [r.model?.id === model.id, r.error, r.warning]; });
        await t("cli.missing", () => resolveCliModel({ cliModel: "zzz-nope-model", modelRuntime: runtime }).error !== undefined);
        await t("scope", async () => { const r = await resolveModelScopeWithDiagnostics([`${model.provider}/${model.id}`], runtime); return [r.scopedModels.map((s) => s.model.id === model.id), r.diagnostics]; });
        const extPath = join(root, "probe-ext.ts");
        write(extPath, `export default function (pi) { pi.registerTool({ name: "probe_tool", label: "P", description: "d", parameters: { type: "object", properties: {} }, async execute() { return { content: [], details: {} }; } }); }`);
        const loaded = await discoverAndLoadExtensions([extPath], cwd, agentDir);
        await t("discover", () => [loaded.extensions.length, [...loaded.extensions[0].tools.keys()], loaded.errors]);
        const extensionsResult = { extensions: loaded.extensions, errors: [], runtime: loaded.runtime };
        const loader = {
            getExtensions: () => extensionsResult,
            getSkills: () => ({ skills: [], diagnostics: [] }),
            getPrompts: () => ({ prompts: [], diagnostics: [] }),
            getThemes: () => ({ themes: [], diagnostics: [] }),
            getAgentsFiles: () => ({ agentsFiles: [] }),
            getSystemPrompt: () => "Custom loader prompt.",
            getSystemPromptSource: () => undefined,
            getAppendSystemPrompt: () => [],
            getAppendSystemPromptSources: () => [],
            extendResources: () => {},
            reload: async () => {},
        };
        const { session } = await createAgentSession({
            cwd, agentDir, resourceLoader: loader, sessionManager: SessionManager.inMemory(cwd),
            settingsManager: SettingsManager.inMemory({}), modelRuntime: runtime, model,
        });
        await session.bindExtensions({});
        await t("custom.loader", () => [session.systemPrompt.startsWith("Custom loader prompt."), session.getAllTools().some((tool) => tool.name === "probe_tool")]);
        const empty = { extensions: [], errors: [], runtime: createExtensionRuntime() };
        const { session: bare } = await createAgentSession({
            cwd, agentDir, resourceLoader: { ...loader, getExtensions: () => empty }, sessionManager: SessionManager.inMemory(cwd),
            settingsManager: SettingsManager.inMemory({}), modelRuntime: runtime, model,
        });
        await t("custom.empty", () => bare.getAllTools().some((tool) => tool.name === "probe_tool"));
        session.dispose();
        bare.dispose();
        finish();
    });
}
"#
    );
    let (result, harness, dir) = run_probe(&source).await;
    let expected: Value = serde_json::from_str(include_str!("fixtures/extensions_sdk_helpers_ts.json")).unwrap();
    assert_eq!(result, expected);
    harness.cleanup();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_session_runtime_matches_ts() {
    let source = format!(
        "{PROBE_PRELUDE}{}",
        r#"
import { mkdtempSync, writeFileSync as writeFile } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import {
    AgentSessionRuntime, createAgentSessionFromServices, createAgentSessionRuntime, createAgentSessionServices,
    SessionManager, wrapRegisteredTool, wrapRegisteredTools,
} from "@earendil-works/pi-coding-agent";
export default function (pi) {
    pi.on("session_start", async (_event, ctx) => {
        try { await probe(ctx); } catch (e) { out.fatal = String(e?.stack ?? e); finish(); }
    });
}
async function probe(ctx) {
    {
        if (globalThis.probeRan) return;
        globalThis.probeRan = true;
        const agentDir = mkdtempSync(join(tmpdir(), "svc-agent-"));
        const cwd = mkdtempSync(join(tmpdir(), "svc-cwd-"));
        const log = [];
        const child = (childPi) => {
            childPi.registerFlag("verbose-x", { type: "boolean", description: "x" });
            childPi.registerFlag("name-x", { type: "string", description: "y" });
            childPi.registerProvider("svc-provider", {
                baseUrl: "http://127.0.0.1:9/v1", apiKey: "svc-key", api: "openai-completions",
                models: [{ id: "svc-model", name: "Svc", reasoning: false, input: ["text"],
                    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 1000, maxTokens: 100 }],
            });
            childPi.registerTool({
                name: "svc_tool", label: "Svc", description: "A child tool", parameters: { type: "object", properties: {} },
                async execute(toolCallId, _params, _signal, _onUpdate, toolCtx) {
                    return { content: [{ type: "text", text: [toolCallId, toolCtx.cwd === cwd, typeof toolCtx.ui.notify, typeof toolCtx.modelRegistry.find].join(",") }], details: {} };
                },
            });
            childPi.on("session_start", (e) => { log.push(["start", e.reason, childPi.getFlag("verbose-x") ?? null, childPi.getFlag("name-x") ?? null]); });
            childPi.on("session_before_switch", (e) => { log.push(["before_switch", e.reason, typeof e.targetSessionFile]); });
            childPi.on("session_before_fork", (e) => { log.push(["before_fork", e.position]); return e.position === "at" ? { cancel: true } : undefined; });
            childPi.on("session_shutdown", (e) => { log.push(["shutdown", e.reason]); });
        };
        const loaderOptions = { extensionFactories: [child], noSkills: true, noPromptTemplates: true, noThemes: true, noContextFiles: true };
        const services = await createAgentSessionServices({
            cwd, agentDir, resourceLoaderOptions: loaderOptions,
            extensionFlagValues: new Map([["verbose-x", "yes"], ["name-x", true], ["unknown-x", true], ["unknown-y", "v"]]),
        });
        await t("services", () => [services.cwd === cwd, services.agentDir === agentDir, Object.keys(services), services.diagnostics]);
        await t("provider", () => services.modelRuntime.getModel("svc-provider", "svc-model")?.id);
        await t("loaderExtensions", () => services.resourceLoader.getExtensions().extensions.map((e) => [...e.flags.keys()]));
        let creates = 0;
        const createRuntime = async ({ cwd, agentDir, sessionManager, sessionStartEvent }) => {
            creates++;
            const services = await createAgentSessionServices({ cwd, agentDir, resourceLoaderOptions: loaderOptions });
            const result = await createAgentSessionFromServices({ services, sessionManager, sessionStartEvent, noTools: "all" });
            return { ...result, services, diagnostics: services.diagnostics };
        };
        const runtime = await createAgentSessionRuntime(createRuntime, { cwd, agentDir, sessionManager: SessionManager.inMemory(cwd) });
        runtime.setRebindSession(async (session) => { log.push(["rebind", session === runtime.session]); await session.bindExtensions({}); });
        runtime.setBeforeSessionInvalidate(() => log.push(["invalidate"]));
        await runtime.session.bindExtensions({});
        await t("runtime", () => [runtime instanceof AgentSessionRuntime, runtime.cwd === cwd, runtime.diagnostics, runtime.services.cwd === cwd, typeof runtime.modelFallbackMessage]);
        const first = runtime.session;
        await t("new", async () => {
            const r = await runtime.newSession({
                setup: async (sm) => { log.push(["setup", typeof sm.appendMessage]); sm.appendMessage({ role: "user", content: [{ type: "text", text: "hello there" }], timestamp: 1 }); },
                withSession: async (rctx) => { log.push(["withSession", typeof rctx.sendUserMessage, typeof rctx.newSession, rctx.cwd === cwd]); },
            });
            return [r, runtime.session !== first, creates];
        });
        const userEntry = runtime.session.sessionManager.getEntries().find((e) => e.type === "message");
        await t("forkAt", () => runtime.fork(userEntry.id, { position: "at" }));
        await t("fork", async () => {
            const r = await runtime.fork(userEntry.id);
            return [r, runtime.session.sessionManager.getEntries().length, creates];
        });
        await t("forkBad", () => runtime.fork("nope"));
        await t("import", async () => {
            try { return await runtime.importFromJsonl(join(cwd, "missing.jsonl")); } catch (e) { return [e.name, e.message.startsWith("File not found: "), e.filePath === join(cwd, "missing.jsonl")]; }
        });
        const file = join(cwd, "other.jsonl");
        writeFile(file, JSON.stringify({ type: "session", version: 3, id: "s-import", timestamp: "2024-01-01T00:00:00.000Z", cwd: join(cwd, "gone") }) + "\n");
        await t("switchMissingCwd", async () => {
            try { await runtime.switchSession(file); return "ok"; } catch (e) { return [e.name, e.message.split(cwd).join("<cwd>").split("\n")[0]]; }
        });
        await t("wrapped", async () => {
            const registered = runtime.services.resourceLoader.getExtensions().extensions[0].tools.get("svc_tool");
            const wrapped = wrapRegisteredTool(registered, runtime.session.extensionRunner);
            const all = wrapRegisteredTools([registered], runtime.session.extensionRunner);
            const result = await wrapped.execute("call-1", {}, undefined, undefined);
            return [Object.keys(registered), Object.keys(wrapped), all.length, result.content[0].text];
        });
        await runtime.dispose();
        await t("log", () => log);
        finish();
    }
}
"#
    );
    let (result, harness, dir) = run_probe(&source).await;
    let expected: Value = serde_json::from_str(include_str!("fixtures/extensions_agent_session_runtime_ts.json")).unwrap();
    assert_eq!(result, expected, "{result:#}");
    harness.cleanup();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_client_drives_a_pi_child() {
    let source = format!(
        "{PROBE_PRELUDE}const PI_BIN = {};\n{}",
        serde_json::to_string(env!("CARGO_BIN_EXE_pi")).unwrap(),
        r#"
import { mkdtempSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { RpcClient } from "@earendil-works/pi-coding-agent";
export default function (pi) {
    pi.on("session_start", async (_event, ctx) => {
        const agentDir = mkdtempSync(join(tmpdir(), "rpc-agent-"));
        // Provider keys in the developer's environment would make a reasoning model available and change the
        // default thinking level; the child must start with no model.
        const blanked = Object.fromEntries(Object.keys(process.env).filter((k) => /API_KEY|TOKEN|SECRET/.test(k)).map((k) => [k, ""]));
        const client = new RpcClient({
            cliPath: PI_BIN, cwd: ctx.cwd, args: ["--no-session"],
            env: { ...blanked, PI_CODING_AGENT_DIR: agentDir, PI_OFFLINE: "1" },
        });
        await t("notStarted", () => client.getState());
        await client.start();
        await t("again", () => client.start());
        const events = [];
        const unsubscribe = client.onEvent((event) => events.push(event.type));
        await t("state", async () => {
            const state = await client.getState();
            return [state.isStreaming, state.messageCount, state.thinkingLevel, typeof state.sessionId];
        });
        await t("name", async () => { await client.setSessionName("rpc#1"); return (await client.getState()).sessionName; });
        await t("bash", async () => {
            const result = await client.bash("echo rpc-ok");
            return [result.output.trim(), result.exitCode, result.cancelled];
        });
        await t("messages", async () => (await client.getMessages()).map((m) => m.role));
        await t("models", async () => Array.isArray(await client.getAvailableModels()));
        await t("commands", async () => Array.isArray(await client.getCommands()));
        await t("badModel", () => client.setModel("nope", "nope"));
        unsubscribe();
        await client.stop();
        await t("stopped", () => client.getState());
        finish();
    });
}
"#
    );
    let (result, harness, dir) = run_probe(&source).await;
    assert_eq!(result["notStarted"], json!("THROW Client not started"), "{result:#}");
    assert_eq!(result["again"], json!("THROW Client already started"), "{result:#}");
    assert_eq!(result["state"], json!([false, 0, "off", "string"]), "{result:#}");
    assert_eq!(result["name"], json!("rpc#1"), "{result:#}");
    assert_eq!(result["bash"], json!(["rpc-ok", 0, false]), "{result:#}");
    assert_eq!(result["messages"], json!(["bashExecution"]), "{result:#}");
    assert_eq!(result["models"], json!(true), "{result:#}");
    assert_eq!(result["commands"], json!(true), "{result:#}");
    assert_eq!(result["badModel"], json!("THROW Model not found: nope/nope"), "{result:#}");
    assert_eq!(result["stopped"], json!("THROW Client not started"), "{result:#}");
    harness.cleanup();
    let _ = std::fs::remove_dir_all(dir);
}
