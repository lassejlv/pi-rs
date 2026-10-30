//! Chunk C: the embedded Session kernel, MemoryStorage and the storage backends whose drivers are
//! JS objects (`storage.*`).

use pi_durable_jsrt::{HostOptions, install};
use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::json;

async fn runtime() -> JsRuntime {
    let rt = JsRuntime::new(RuntimeOptions {
        timer_control: true,
        ..Default::default()
    })
    .unwrap();
    install(&rt, HostOptions::default()).await.unwrap();
    rt
}

async fn run(rt: &JsRuntime, src: &str) -> serde_json::Value {
    let ns = rt
        .eval_module(src, "/session-embedded-test.mjs")
        .await
        .unwrap();
    ns.call_method("run", vec![]).await.unwrap()
}

/// A minimal upstream-shaped `SqliteDatabase` over `node:sqlite`, with a settlement hook.
const DRIVER: &str = r#"
import { DatabaseSync } from "node:sqlite";
class Executor {
  constructor(db) { this.db = db; }
  async exec(sql) { this.db.exec(sql); }
  async run(sql, ...params) { this.db.prepare(sql).run(...params); }
  async get(sql, ...params) { return this.db.prepare(sql).get(...params); }
  async all(sql, ...params) { return this.db.prepare(sql).all(...params); }
}
export class Driver extends Executor {
  calls = 0;
  constructor() { super(new DatabaseSync(":memory:")); }
  async transaction(callback) {
    this.calls++;
    this.db.exec("BEGIN IMMEDIATE");
    try {
      const value = await callback(new Executor(this.db));
      this.db.exec("COMMIT");
      return value;
    } catch (error) {
      this.db.exec("ROLLBACK");
      throw error;
    }
  }
  async close() { this.db.close(); }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_storage_runs_over_a_js_driver() {
    let rt = runtime().await;
    let out = run(
        &rt,
        &format!(
            r#"{DRIVER}
        import {{ SqliteStorage, CURRENT_SQLITE_SCHEMA_VERSION, SQLITE_MIGRATIONS, applySqliteMigrations }} from "@earendil-works/pi-durable/storage/sqlite";
        const context = {{ abortSignal: undefined, value() {{ return undefined; }} }};
        export async function run() {{
            const driver = new Driver();
            const storage = await SqliteStorage.open(driver);
            const adopted = [];
            const seq = await storage.commit([{{ type: "conversation", value: {{ id: 1 }} }}, {{ type: "entry", value: {{ id: 2, conversationId: 1, kind: "note", data: "\ud800" }} }}], (s) => adopted.push(s), context);
            const entry = await storage.entry(2, context);
            const scan = await storage.scanEntries({{ conversationId: 1 }}, 10, undefined, context);
            let duplicate;
            try {{ await storage.commit([{{ type: "conversation", value: {{ id: 1 }} }}], () => {{}}, context); }} catch (e) {{ duplicate = e.message; }}
            const failure = new Error("adopt failed");
            let thrown;
            try {{ await storage.commit([{{ type: "entry", value: {{ id: 3, conversationId: 1, kind: "x" }} }}], () => {{ throw failure; }}, context); }} catch (e) {{ thrown = e === failure; }}
            const closing = storage.close(context);
            const same = storage.close(context) === closing;
            await closing;
            let closed;
            try {{ await storage.mintId(); }} catch (e) {{ closed = e.message; }}
            const migrated = new Driver();
            await applySqliteMigrations(migrated, SQLITE_MIGRATIONS.slice(0, 1));
            const version = (await migrated.get("SELECT version FROM durable_schema WHERE singleton = 1")).version;
            return {{ seq, adopted, kind: entry.entry.kind, lone: entry.entry.data.charCodeAt(0), items: scan.items.length, duplicate: typeof duplicate, thrown, same, closed, transactions: driver.calls > 0, current: CURRENT_SQLITE_SCHEMA_VERSION, version }};
        }}
        "#
        ),
    )
    .await;
    assert_eq!(
        out,
        json!({
            "seq": 1, "adopted": [1], "kind": "note", "lone": 0xd800, "items": 1, "duplicate": "string",
            "thrown": true, "same": true, "closed": "SqliteStorage is closed", "transactions": true,
            "current": 2, "version": 1
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn jsonl_storage_runs_over_a_js_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime().await;
    let src = r#"
        import * as fs from "node:fs/promises";
        import * as path from "node:path";
        import { JsonlStorage } from "@earendil-works/pi-durable/storage/jsonl";
        const context = { abortSignal: undefined, value() { return undefined; } };
        const ok = (value) => ({ ok: true, value });
        const fail = (e, p) => ({ ok: false, error: { code: e.code === "ENOENT" ? "not_found" : "unknown", message: e.message, path: p } });
        const wrap = (p, f) => f().then(ok, (e) => fail(e, p));
        class Fs {
          id = "test"; cwd = DIR; calls = 0;
          absolutePath(p) { this.calls++; return Promise.resolve(ok(path.resolve(this.cwd, p))); }
          joinPath(parts) { return Promise.resolve(ok(path.join(...parts))); }
          readBinaryFile(p) { this.calls++; return wrap(p, async () => new Uint8Array(await fs.readFile(p))); }
          writeFile(p, c) { this.calls++; return wrap(p, () => fs.writeFile(p, c)); }
          appendFile(p, c) { this.calls++; return wrap(p, () => fs.appendFile(p, c)); }
          truncateFile(p, n) { return wrap(p, () => fs.truncate(p, n)); }
          flushFile(p) { return Promise.resolve(ok(undefined)); }
          renameFile(a, b) { return wrap(a, () => fs.rename(a, b)); }
          listDir(p) { return wrap(p, async () => (await fs.readdir(p, { withFileTypes: true })).map((d) => ({ name: d.name, path: path.join(p, d.name), kind: d.isDirectory() ? "directory" : "file", size: 0, mtimeMs: 0 }))); }
          createDir(p, o) { return wrap(p, () => fs.mkdir(p, o)); }
          remove(p, o) { return wrap(p, () => fs.rm(p, o)); }
        }
        export async function run() {
          const files = new Fs();
          const storage = await JsonlStorage.open(path.join(DIR, "store"), files, context);
          const seq = await storage.commit([{ type: "conversation", value: { id: 1 } }], () => {}, context);
          await storage.close(context);
          const reopened = await JsonlStorage.open(path.join(DIR, "store"), files, context);
          const conversation = await reopened.conversation(1, context);
          await reopened.close(context);
          return { seq, conversation, used: files.calls > 0 };
        }
    "#
    .replace("DIR", &format!("{:?}", dir.path().to_string_lossy()));
    let out = run(&rt, &src).await;
    assert_eq!(
        out,
        json!({ "seq": 1, "conversation": { "id": 1 }, "used": true })
    );
}

/// The embedded counterpart of upstream `storage-runtime-boundary.test.ts` (which inspects the
/// upstream sources): the portable entry modules' static import graphs within the embedded
/// package import no `node:` builtin and do not reach their Node-only siblings.
#[test]
fn portable_entry_modules_import_no_node_builtins() {
    use std::collections::{BTreeMap, BTreeSet};
    let modules: BTreeMap<String, &str> = pi_durable_jsrt::modules(false).into_iter().collect();
    let specifiers = |source: &str| -> Vec<String> {
        let mut out = Vec::new();
        for statement in source.split(';') {
            let statement = statement.trim_start();
            if !(statement.starts_with("import") || statement.starts_with("export")) {
                continue;
            }
            if let Some(end) = statement.rfind(['"', '\'']) {
                let quote = &statement[end..=end];
                if let Some(start) = statement[..end].rfind(quote) {
                    let tail = statement[..start].trim_end();
                    if tail.ends_with("from") || tail.ends_with("import") {
                        out.push(statement[start + 1..end].to_string());
                    }
                }
            }
        }
        out
    };
    let graph = |entry: &str| -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut pending = vec![entry.to_string()];
        while let Some(spec) = pending.pop() {
            if !seen.insert(spec.clone()) {
                continue;
            }
            let source = modules
                .get(&spec)
                .unwrap_or_else(|| panic!("{spec} is not embedded"));
            for import in specifiers(source) {
                assert!(
                    !import.starts_with("node:"),
                    "{spec} imports the Node built-in {import}"
                );
                if modules.contains_key(&import) {
                    pending.push(import);
                }
            }
        }
        seen
    };
    let root = graph("@earendil-works/pi-durable");
    // The walk follows the glue: the root reaches the Session and the Harness modules.
    assert!(root.contains("pi-durable:internal/src/durable/session/session"));
    assert!(root.contains("pi-durable:internal/harness"));
    for excluded in ["/env", "/storage/jsonl", "/storage/sqlite"] {
        assert!(
            !root
                .iter()
                .any(|m| m.starts_with(&format!("@earendil-works/pi-durable{excluded}"))),
            "the package root reaches {excluded}"
        );
    }
    assert!(
        !graph("@earendil-works/pi-durable/storage/sqlite")
            .contains("@earendil-works/pi-durable/storage/sqlite/node")
    );
    assert!(
        !graph("@earendil-works/pi-durable/env").contains("@earendil-works/pi-durable/env/node")
    );
    let jsonl = graph("@earendil-works/pi-durable/storage/jsonl");
    assert!(!jsonl.contains("@earendil-works/pi-durable/storage/jsonl/node"));
    assert!(!jsonl.contains("@earendil-works/pi-durable/env/node"));
}

/// The pi-ai functions the Harness glue imports, for a runtime without pi-ai.
const PI_AI_STUB: &str = r#"
export const mergeClientIdentity = (base, override) => ({ ...base, ...override });
export const withClientIdentity = (models) => models;
export const validateToolArguments = (_tool, call) => call.arguments;
let next = 0;
export const uuidv7 = () => `00000000-0000-7000-8000-${String(++next).padStart(12, "0")}`;
"#;

/// `@earendil-works/pi-telemetry` for a runtime without the reference checkout: a no-op span.
const PI_TELEMETRY_STUB: &str = r#"
export const NOOP_TELEMETRY_CONTEXT = Object.freeze({});
"#;

fn stub_packages(rt: &JsRuntime) {
    for spec in [
        "@earendil-works/pi-ai",
        "@earendil-works/pi-ai/utils/identity",
        "@earendil-works/pi-ai/utils/validation",
    ] {
        rt.set_virtual_module(spec, pi_jsrt::VirtualModule::EsmJs(PI_AI_STUB.to_string()));
    }
    rt.set_virtual_module(
        "@earendil-works/pi-telemetry",
        pi_jsrt::VirtualModule::EsmJs(PI_TELEMETRY_STUB.to_string()),
    );
}

async fn session_runtime() -> JsRuntime {
    let rt = JsRuntime::new(RuntimeOptions {
        timer_control: true,
        ..Default::default()
    })
    .unwrap();
    stub_packages(&rt);
    install(&rt, HostOptions::default()).await.unwrap();
    rt
}

/// Regression: a commit's Context reaches its watch delivery even when the commit request's
/// `context.drop` frame arrives in the same transport batch as the (deferred) watch callback.
#[tokio::test(flavor = "multi_thread")]
async fn watch_delivery_keeps_the_commit_context() {
    let rt = session_runtime().await;
    let out = run(
        &rt,
        r#"
        import { defineDoc, MemoryStorage } from "@earendil-works/pi-durable";
        import { SessionImpl } from "pi-durable:internal/src/durable/session/session";
        import { BACKGROUND_CONTEXT, createContextKey, withContextValue } from "@earendil-works/chord/context";
        const Doc = defineDoc({ checkpointWhen: () => false, kind: "c.watch", version: 1, scope: "session", initial: () => ({ value: 0 }) });
        export async function run() {
            const session = new SessionImpl(new MemoryStorage(), Date.now, () => 600000);
            await session.commit((tx) => tx.doc(Doc).then(() => undefined), BACKGROUND_CONTEXT);
            const watch = await session.watchDoc(Doc, BACKGROUND_CONTEXT);
            const key = createContextKey("c.key");
            const seen = [];
            let delivered;
            watch.start(async (_value, _ops, context) => { seen.push(context.value(key)); delivered?.(); });
            for (let i = 1; i <= 20; i++) {
                const arrived = new Promise((resolve) => { delivered = resolve; });
                await session.commit(async (tx) => { (await tx.doc(Doc)).value = i; }, withContextValue(key, `commit-${i}`, BACKGROUND_CONTEXT));
                await arrived;
            }
            await watch.stop();
            await session.close(BACKGROUND_CONTEXT);
            return seen.every((value, index) => value === `commit-${index + 1}`) ? "ok" : seen;
        }
        "#,
    )
    .await;
    assert_eq!(out, json!("ok"));
}

/// Stress for the once-seen hang where a commit callback never resumed after `await tx.doc()`:
/// many sessions commit concurrently through the in-process transport, each commit bounded.
#[tokio::test(flavor = "multi_thread")]
async fn commit_callbacks_resume_after_tx_doc_under_load() {
    let rt = session_runtime().await;
    let out = run(
        &rt,
        r#"
        import { defineDoc, MemoryStorage } from "@earendil-works/pi-durable";
        import { SessionImpl } from "pi-durable:internal/src/durable/session/session";
        import { BACKGROUND_CONTEXT as context } from "@earendil-works/chord/context";
        const Doc = defineDoc({ checkpointWhen: () => false, kind: "c.stress", version: 1, scope: "session", initial: () => ({ text: "" }) });
        class Controlled extends MemoryStorage {
            async commit(writes, adopt, c) { await Promise.resolve(); return super.commit(writes, adopt, c); }
        }
        const bounded = (promise, label) => Promise.race([promise, new Promise((_, reject) => setTimeout(() => reject(new Error(`hung: ${label}`)), 5000))]);
        export async function run() {
            const sessions = Array.from({ length: 8 }, () => new SessionImpl(new Controlled(), Date.now, () => 600000));
            await Promise.all(sessions.map(async (session, s) => {
                for (let i = 0; i < 60; i++) {
                    await bounded(session.commit(async (tx) => {
                        await tx.scanConversations({}, 1);
                        (await tx.doc(Doc)).text = `${s}:${i}`;
                    }, context), `session ${s} commit ${i}`);
                }
            }));
            const texts = await Promise.all(sessions.map((session) => session.snapshot(Doc, context).then((v) => v.text)));
            await Promise.all(sessions.map((session) => session.close(context)));
            return texts;
        }
        "#,
    )
    .await;
    assert_eq!(
        out,
        json!((0..8).map(|s| format!("{s}:59")).collect::<Vec<_>>())
    );
}

/// Host round trips per commit in the upstream retention case's shape (a ControlledStorage-like
/// subclass, JS clock and retention): every frame JS sends the in-process service is a request
/// or a callback's reply, so `transport.send` calls count round trips. Wall time is logged only
/// (it depends on machine load); the round-trip budget is asserted.
#[tokio::test(flavor = "multi_thread")]
async fn commit_round_trips_stay_within_budget() {
    let rt = JsRuntime::new(RuntimeOptions {
        timer_control: true,
        ..Default::default()
    })
    .unwrap();
    stub_packages(&rt);
    let host = install(&rt, HostOptions::default()).await.unwrap();
    let ns = rt
        .eval_module(
            r#"
        import { defineDoc, MemoryStorage } from "@earendil-works/pi-durable";
        import { SessionImpl } from "pi-durable:internal/src/durable/session/session";
        import { BACKGROUND_CONTEXT as context } from "@earendil-works/chord/context";
        const Doc = defineDoc({ checkpointWhen: () => false, kind: "c.bench", version: 1, scope: "conversation", history: "latest", fork: "current", initial: () => ({ count: 0 }) });
        class Controlled extends MemoryStorage {
            async commit(writes, adopt, c) { return super.commit(writes, adopt, c); }
            mintId() { return super.mintId(); }
        }
        const clock = { now: 1_000 };
        let session;
        export async function open() {
            session = new SessionImpl(new Controlled(), () => clock.now, () => 10_000);
            await session.commit(async () => {}, context);
        }
        export async function run() {
            const started = performance.now();
            for (let n = 0; n < 300; n++) {
                const id = await session.commit(async (tx) => (await tx.createConversation({ ownership: { kind: "ownerless" } })).id, context);
                await session.commit(async (tx) => { (await tx.doc(Doc, id)).count++; }, context);
                clock.now += 1_000;
            }
            return { msPerCommit: (performance.now() - started) / 600 };
        }
        export async function close() { await session.close(context); }
        "#,
            "/session-embedded-round-trips.mjs",
        )
        .await
        .unwrap();
    ns.call_method("open", vec![]).await.unwrap();
    let sends = || host.counters().get("transport.send").copied().unwrap_or(0);
    let before = sends();
    let timing = ns.call_method("run", vec![]).await.unwrap();
    let per_commit = (sends() - before) as f64 / 600.0;
    ns.call_method("close", vec![]).await.unwrap();
    eprintln!("commit round trips: {per_commit:.2} per commit, {timing}");
    assert!(
        per_commit <= 15.0,
        "{per_commit:.2} host round trips per commit (budget 15)"
    );
}

/// A test that times out leaves its commits running; the shim aborts its context `signal` (as
/// vitest does), so work that honours it stops before the next test, which then sees no commits
/// from the timed-out one.
#[tokio::test(flavor = "multi_thread")]
async fn timed_out_test_commits_stop_before_the_next_test() {
    let rt = JsRuntime::new(RuntimeOptions {
        timer_control: true,
        ..Default::default()
    })
    .unwrap();
    stub_packages(&rt);
    install(
        &rt,
        HostOptions {
            vitest: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    rt.eval_module(
        r#"
        import { it, expect } from "vitest";
        import { defineDoc, MemoryStorage } from "@earendil-works/pi-durable";
        import { SessionImpl } from "pi-durable:internal/src/durable/session/session";
        import { BACKGROUND_CONTEXT as context } from "@earendil-works/chord/context";
        const Doc = defineDoc({ checkpointWhen: () => false, kind: "c.leak", version: 1, scope: "session", initial: () => ({ n: 0 }) });
        class Counting extends MemoryStorage {
            commits = 0;
            async commit(writes, adopt, c) { this.commits++; return super.commit(writes, adopt, c); }
        }
        const storage = new Counting();
        const session = new SessionImpl(storage, Date.now, () => 600000);
        let leakedStopped = false;
        it("runs past its deadline", { timeout: 200 }, async ({ signal }) => {
            while (!signal.aborted) await session.commit(async (tx) => { (await tx.doc(Doc)).n++; }, context);
            leakedStopped = true;
        });
        it("sees no commits from the timed-out test", async () => {
            await new Promise((resolve) => setTimeout(resolve, 50));
            const before = storage.commits;
            await new Promise((resolve) => setTimeout(resolve, 200));
            expect(leakedStopped).toBe(true);
            expect(storage.commits).toBe(before);
            await session.close(context);
        });
        "#,
        "/leak.test.mjs",
    )
    .await
    .unwrap();
    let runner = rt
        .import("vitest")
        .await
        .unwrap()
        .get_handle("__runner")
        .await
        .unwrap();
    let report = runner
        .call_method("run", vec![json!({ "file": "leak" })])
        .await
        .unwrap();
    let statuses: Vec<String> = report["testResults"][0]["assertionResults"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["status"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(statuses, ["failed", "passed"], "{report:#}");
}

/// The native IndexedDbStorage over fake-indexeddb (upstream's pinned test dependency, installed
/// in `tests/indexeddb/` with `npm ci`): one readwrite transaction stays active through a commit
/// of many writes, and the database reopens with its rows.
#[tokio::test(flavor = "multi_thread")]
async fn indexeddb_storage_runs_over_a_js_indexeddb() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/indexeddb");
    if !dir.join("node_modules/fake-indexeddb").is_dir() {
        // The gate installs it; a plain `cargo test` skips rather than fails.
        assert!(
            std::env::var_os("PI_DURABLE_REQUIRE_INDEXEDDB").is_none(),
            "run `npm ci` in {} (fake-indexeddb, the TS reference's pinned test dependency)",
            dir.display()
        );
        eprintln!("skipped: run `npm ci` in {} to enable this test", dir.display());
        return;
    }
    let rt = JsRuntime::new(RuntimeOptions {
        timer_control: true,
        cwd: Some(dir.clone()),
        ..Default::default()
    })
    .unwrap();
    stub_packages(&rt);
    install(&rt, HostOptions::default()).await.unwrap();
    let ns = rt
        .eval_module(
            r#"
        import { IDBFactory, IDBKeyRange } from "fake-indexeddb";
        import { IndexedDbStorage } from "@earendil-works/pi-durable/storage/indexeddb";
        const context = { abortSignal: undefined, value() { return undefined; } };
        globalThis.IDBKeyRange = IDBKeyRange;
        export async function run() {
            const indexedDB = new IDBFactory();
            const storage = await IndexedDbStorage.open("c", { indexedDB });
            await storage.commit([{ type: "conversation", value: { id: 1 } }], () => {}, context);
            const writes = [];
            for (let i = 0; i < 40; i++) writes.push({ type: "entry", value: { id: await storage.mintId(), conversationId: 1, kind: "bulk", data: { i } } });
            const adopted = [];
            const seq = await storage.commit(writes, (s) => adopted.push(s), context);
            await storage.close(context);
            const reopened = await IndexedDbStorage.open("c", { indexedDB });
            const scan = await reopened.scanEntries({ conversationId: 1 }, 100, undefined, context);
            await reopened.close(context);
            return { seq, adopted, entries: scan.items.length };
        }
        "#,
            dir.join("idb-test.mjs").display().to_string(),
        )
        .await
        .unwrap();
    let out = ns.call_method("run", vec![]).await.unwrap();
    assert_eq!(out, json!({ "seq": 2, "adopted": [2], "entries": 40 }));
}
