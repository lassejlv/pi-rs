//! Chunk E: the embedded environment (`/env`, `/env/node`), built-in tools (`/tools`), images
//! (`/images*`) and conformance drivers (`/testing`) over native pi-durable.

use pi_durable_jsrt::{DurableHost, HostOptions, install};
use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::{Value, json};

/// `@earendil-works/chord/context` imports `@earendil-works/pi-telemetry`, which resolves from
/// the reference checkout's `node_modules` (`PI_MONO`, default `../pi-mono` beside the workspace).
fn package_dir() -> Option<std::path::PathBuf> {
    let reference = std::env::var_os("PI_MONO")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../pi-mono")
        });
    let dir = reference.join("packages/durable");
    dir.is_dir().then_some(dir)
}

async fn runtime() -> (JsRuntime, DurableHost) {
    let rt = JsRuntime::new(RuntimeOptions {
        timer_control: true,
        cwd: package_dir(),
        ..Default::default()
    })
    .unwrap();
    let host = install(&rt, HostOptions::default()).await.unwrap();
    (rt, host)
}

async fn run(rt: &JsRuntime, name: &str, src: &str) -> Value {
    let ns = rt
        .eval_module(src, format!("/env-embedded-{name}.mjs"))
        .await
        .unwrap();
    ns.call_method("run", vec![]).await.unwrap()
}

fn calls(host: &DurableHost, prefix: &str) -> u64 {
    host.counters()
        .iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .map(|(_, n)| n)
        .sum()
}

const PRELUDE: &str = r#"
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BACKGROUND_CONTEXT as context, withAbortSignal } from "@earendil-works/chord/context";
import { getOrThrow, FileError, ExecutionError } from "@earendil-works/pi-durable/env";
import { NodeExecutionEnv } from "@earendil-works/pi-durable/env/node";
const temp = () => mkdtempSync(join(tmpdir(), "pi-durable-env-embedded-"));
const aborted = () => { const c = new AbortController(); c.abort(); return withAbortSignal(c.signal, context); };
"#;

#[tokio::test(flavor = "multi_thread")]
async fn node_execution_env_runs_files_readers_and_shell_natively() {
    let (rt, host) = runtime().await;
    let out = run(
        &rt,
        "node",
        &format!(
            r#"{PRELUDE}
        export async function run() {{
            const root = temp();
            try {{
                const env = new NodeExecutionEnv({{ cwd: root }});
                getOrThrow(await env.writeFile("dir/a.txt", "one\ntwo\r\nthree", context));
                const text = getOrThrow(await env.readTextFile("dir/a.txt", context));
                const lines = getOrThrow(await env.readTextLines("dir/a.txt", {{ maxLines: 2 }}, context));
                const reader = getOrThrow(await env.openBinaryReader("dir/a.txt", undefined, context));
                const scan = getOrThrow(await reader.scanLines({{ startLine: 1 }}, context));
                const bytes = [...getOrThrow(await reader.read(0, 3, context))];
                await reader.close(context);
                const missing = await env.fileInfo("nope", context);
                const pre = await env.readTextFile("dir/a.txt", aborted());
                let output = "";
                const exec = getOrThrow(await env.exec("printf hi; printf err >&2; exit 3", {{ onOutput: (t) => {{ output += t; }} }}, context));
                const timeout = await env.exec("sleep 5", {{ timeout: 0.05 }}, context);
                const failing = await env.exec("printf x", {{ onOutput: () => {{ throw new Error("boom"); }} }}, context);
                const spill = getOrThrow(await env.exec("printf 12345678901234567890", {{ spill: {{ afterBytes: 5, afterLines: 5 }} }}, context));
                const spilled = getOrThrow(await env.readTextFile(spill.spillPath, context));
                rmSync(join(spill.spillPath, ".."), {{ recursive: true, force: true }});
                return {{
                    id: env.id, text, lines, scan, bytes, spilled, output, exitCode: exec.exitCode,
                    missing: missing.ok ? null : {{ isFileError: missing.error instanceof FileError, code: missing.error.code }},
                    aborted: pre.ok ? null : pre.error.code,
                    timeout: timeout.ok ? null : {{ isExecutionError: timeout.error instanceof ExecutionError, code: timeout.error.code }},
                    callback: failing.ok ? null : [failing.error.code, failing.error.message],
                }};
            }} finally {{ rmSync(root, {{ recursive: true, force: true }}); }}
        }}"#
        ),
    )
    .await;
    assert_eq!(
        out,
        json!({
            "id": "node:local", "text": "one\ntwo\r\nthree", "lines": ["one", "two\r"],
            "scan": { "newlines": 2, "start": 4, "end": 14, "firstLineEnd": 8, "lastLineStart": 9, "selectedBytes": 10, "firstLineBytes": 4 },
            "bytes": [111, 110, 101], "spilled": "12345678901234567890", "output": "hierr", "exitCode": 3,
            "missing": { "isFileError": true, "code": "not_found" }, "aborted": "aborted",
            "timeout": { "isExecutionError": true, "code": "timeout" }, "callback": ["callback_error", "boom"],
        })
    );
    assert!(calls(&host, "fs.") > 10 && calls(&host, "reader.") >= 3);
    rt.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn decoders_and_line_scanner_are_native() {
    let (rt, host) = runtime().await;
    let out = run(
        &rt,
        "decode",
        r#"
        import { StreamDecoder, LineScanner, rangeDecoder, startsWithBom } from "@earendil-works/pi-durable/env";
        export async function run() {
            const decoder = new StreamDecoder();
            const bytes = [0xef, 0xbb, 0xbf, 0xe2, 0x82, 0xac, 0x61];
            const text = bytes.map((b) => decoder.decode(Uint8Array.of(b))).join("") + decoder.decode();
            const range = rangeDecoder();
            const kept = range.decode(Uint8Array.of(0xef, 0xbb, 0xbf, 0x62));
            const scanner = new LineScanner(1, 2);
            scanner.push(new TextEncoder().encode("a\nbc\nd"));
            let invalid;
            try { new LineScanner(-1); } catch (e) { invalid = [e instanceof RangeError, e.message]; }
            return { text, kept, bom: startsWithBom(Uint8Array.of(0xef, 0xbb, 0xbf)), scan: scanner.finish(), invalid };
        }"#,
    )
    .await;
    assert_eq!(out["text"], "€a");
    assert_eq!(out["kept"], "\u{feff}b");
    assert_eq!(out["bom"], true);
    assert_eq!(out["scan"]["start"], 2);
    assert_eq!(out["scan"]["end"], 4);
    assert_eq!(out["invalid"], json!([true, "Invalid line range"]));
    assert!(calls(&host, "env.") > 0);
    rt.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn native_tools_use_the_callers_js_environment_and_api() {
    let (rt, host) = runtime().await;
    let out = run(
        &rt,
        "tools",
        &format!(
            r#"{PRELUDE}
        import {{ createReadTool, createWriteTool, createEditTool, createBashTool, createCodingTools }} from "@earendil-works/pi-durable/tools";
        class CountingEnv extends NodeExecutionEnv {{
            reads = 0;
            async readTextFile(path, context) {{ this.reads++; return super.readTextFile(path, context); }}
        }}
        export async function run() {{
            const root = temp();
            try {{
                const env = new CountingEnv({{ cwd: root }});
                const output = [];
                const diagnostics = [];
                const api = {{
                    env, output: (text) => output.push(text), retainedOutput: () => ({{ text: output.join(""), truncated: false }}),
                    diagnostic: (d) => diagnostics.push(d), agent: async () => ({{}}),
                }};
                const write = await createWriteTool().execute({{ path: "f.txt", content: "alpha\nbeta\n" }}, api, context);
                const edit = await createEditTool().execute({{ path: "f.txt", edits: [{{ oldText: "beta", newText: "gamma" }}] }}, api, context);
                const read = await createReadTool().execute({{ path: "f.txt" }}, api, context);
                let prepared;
                const bash = await createBashTool({{ prepare: (execution) => {{ prepared = execution.command; execution.command = "printf prepared"; }} }})
                    .execute({{ command: "printf original" }}, api, context);
                let missing;
                try {{ await createEditTool().execute({{ path: "none.txt", edits: [{{ oldText: "a", newText: "b" }}] }}, api, context); }} catch (e) {{ missing = e.message; }}
                return {{
                    write: write.output, edit: edit.output, details: typeof edit.details.diff, read: read.output,
                    bash: bash.output, prepared, streamed: output.join(""), reads: env.reads, missing,
                    tools: createCodingTools().tools.map((t) => t.name), schema: createReadTool().parameters.required,
                }};
            }} finally {{ rmSync(root, {{ recursive: true, force: true }}); }}
        }}"#
        ),
    )
    .await;
    assert_eq!(
        out["write"],
        json!([{ "type": "text", "text": "Successfully wrote to f.txt" }])
    );
    assert_eq!(
        out["edit"],
        json!([{ "type": "text", "text": "Successfully replaced 1 block(s) in f.txt." }])
    );
    assert_eq!(out["details"], "string");
    assert_eq!(
        out["read"],
        json!([{ "type": "text", "text": "alpha\ngamma\n" }])
    );
    assert_eq!(out["prepared"], "printf original");
    assert_eq!(out["streamed"], "prepared");
    assert!(
        out["reads"].as_u64().unwrap() >= 1,
        "edit read through the subclass: {out}"
    );
    assert!(out["missing"].as_str().unwrap().contains("none.txt"));
    assert_eq!(out["tools"], json!(["read", "write", "edit", "bash"]));
    assert_eq!(out["schema"], json!(["path"]));
    assert!(calls(&host, "builtin.") >= 5);
    rt.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn file_mutation_queue_orders_mutations_and_keeps_thrown_values() {
    let (rt, _host) = runtime().await;
    let out = run(
        &rt,
        "queue",
        &format!(
            r#"{PRELUDE}
        import {{ withFileMutationQueue }} from "pi-durable:internal/src/durable/tools/file-mutation-queue";
        export async function run() {{
            const root = temp();
            try {{
                const env = new NodeExecutionEnv({{ cwd: root }});
                let release;
                const gate = new Promise((r) => (release = r));
                const order = [];
                let started;
                const entered = new Promise((r) => (started = r));
                const first = withFileMutationQueue(env, "x", async () => {{ order.push("first:start"); started(); await gate; order.push("first:end"); return 1; }}, context);
                // Calls run in the order their keys resolve (upstream's contract): queue the second once the first holds the file.
                await entered;
                const second = withFileMutationQueue(env, "./x", async () => {{ order.push("second"); return {{ two: 2 }}; }}, context);
                await new Promise((r) => setTimeout(r, 20));
                order.push("released");
                release();
                const results = await Promise.all([first, second]);
                const failure = new Error("mine");
                let same;
                try {{ await withFileMutationQueue(env, "x", async () => {{ throw failure; }}, context); }} catch (e) {{ same = e === failure; }}
                return {{ order, results, same }};
            }} finally {{ rmSync(root, {{ recursive: true, force: true }}); }}
        }}"#
        ),
    )
    .await;
    assert_eq!(
        out,
        json!({ "order": ["first:start", "released", "first:end", "second"], "results": [1, { "two": 2 }], "same": true })
    );
    rt.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn images_prepare_with_native_photon() {
    let (rt, host) = runtime().await;
    let out = run(
        &rt,
        "images",
        r#"
        import { createPhotonImages } from "@earendil-works/pi-durable/images";
        import { createNodePhotonImages } from "@earendil-works/pi-durable/images/node";
        import { createCloudflarePhotonImages } from "@earendil-works/pi-durable/images/cloudflare";
        import { DEFAULT_IMAGE_LIMITS } from "@earendil-works/pi-durable/tools";
        import { detectSupportedImageMimeType, imageDimensions } from "pi-durable:internal/src/durable/tools/image";
        import { toBase64, base64Length } from "pi-durable:internal/src/durable/tools/image-processor";
        import { exifOrientation } from "pi-durable:internal/src/durable/images/exif";
        const gif = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
        const bytes = () => Uint8Array.from(atob(gif), (c) => c.charCodeAt(0));
        export async function run() {
            const inline = await createPhotonImages(undefined).prepare(bytes(), "image/gif", DEFAULT_IMAGE_LIMITS);
            const worker = await (await createNodePhotonImages({ idleMs: 10 })).prepare(bytes(), "image/gif", DEFAULT_IMAGE_LIMITS);
            const cloudflare = await createCloudflarePhotonImages().prepare(bytes(), "image/gif", DEFAULT_IMAGE_LIMITS);
            const broken = await createPhotonImages().prepare(Uint8Array.of(1, 2, 3), "image/png", DEFAULT_IMAGE_LIMITS);
            return {
                inline, worker, cloudflare, broken: broken === undefined,
                mime: detectSupportedImageMimeType(bytes()), dimensions: imageDimensions(bytes(), "image/gif"),
                base64: toBase64(bytes()) === gif, length: base64Length(4), exif: exifOrientation(bytes()),
            };
        }"#,
    )
    .await;
    let prepared = json!({ "data": "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7", "mimeType": "image/gif" });
    assert_eq!(out["inline"], prepared);
    assert_eq!(out["worker"], prepared);
    assert_eq!(out["cloudflare"], prepared);
    assert_eq!(out["broken"], true);
    assert_eq!(out["mime"], "image/gif");
    assert_eq!(out["dimensions"], json!({ "width": 1, "height": 1 }));
    assert_eq!(out["base64"], true);
    assert_eq!(out["length"], 8);
    assert_eq!(out["exif"], 1);
    assert!(calls(&host, "image.") >= 4);
    rt.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn truncate_helpers_are_native() {
    let (rt, _host) = runtime().await;
    let out = run(
        &rt,
        "truncate",
        r#"
        import { formatSize, truncateHead, DEFAULT_MAX_LINES } from "pi-durable:internal/src/durable/truncate";
        export async function run() {
            return { sizes: [formatSize(10), formatSize(1536), formatSize(3 * 1024 * 1024)],
                head: truncateHead("one\ntwo\nthree", { maxLines: 2 }), max: DEFAULT_MAX_LINES };
        }"#,
    )
    .await;
    assert_eq!(out["sizes"], json!(["10B", "1.5KB", "3.0MB"]));
    assert_eq!(out["head"]["content"], "one\ntwo");
    assert_eq!(out["head"]["truncatedBy"], "lines");
    assert_eq!(out["max"], 2000);
    rt.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn env_conformance_runs_native_cases_against_a_js_environment() {
    let (rt, host) = runtime().await;
    let out = run(
        &rt,
        "conformance",
        &format!(
            r#"{PRELUDE}
        import {{ createEnvConformance, createExpectAssertions }} from "@earendil-works/pi-durable/testing";
        export async function run() {{
            let provided = 0;
            const cases = createEnvConformance({{
                assertions: createExpectAssertions(() => ({{}})),
                withEnv: async (use) => {{
                    const cwd = temp();
                    provided++;
                    try {{ await use(new NodeExecutionEnv({{ cwd }})); }} finally {{ rmSync(cwd, {{ recursive: true, force: true }}); }}
                }},
            }});
            const failures = [];
            for (const testCase of cases) {{
                if (testCase.name.startsWith("watch ")) continue;
                try {{ await testCase.run(); }} catch (e) {{ failures.push(`${{testCase.name}}: ${{e.message}}`); }}
            }}
            return {{ names: cases.map((c) => c.name), watchTimeouts: cases.filter((c) => c.timeoutMs === 30000).length, provided, failures }};
        }}"#
        ),
    )
    .await;
    assert_eq!(out["failures"], json!([]), "{out}");
    let names = out["names"].as_array().unwrap();
    assert!(
        names
            .iter()
            .any(|n| n == "binary reader reads byte ranges of the opened file")
    );
    assert!(
        !names
            .iter()
            .any(|n| n.as_str().unwrap().starts_with("listFiles "))
    );
    assert!(out["watchTimeouts"].as_u64().unwrap() >= 8);
    assert!(out["provided"].as_u64().unwrap() >= 10);
    assert!(calls(&host, "testing.envUse") >= 10);
    rt.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_benchmarks_run_on_memory_and_sqlite() {
    let (rt, host) = runtime().await;
    let out = run(
        &rt,
        "bench",
        &format!(
            r#"{PRELUDE}
        import {{ MemoryStorage }} from "@earendil-works/pi-durable/storage/memory";
        import {{ openNodeSqliteStorage }} from "@earendil-works/pi-durable/storage/sqlite/node";
        import {{ seedStorageBenchmark, seedStorageWriteBenchmark, STORAGE_READ_BENCHMARKS, STORAGE_WRITE_BENCHMARKS,
            STORAGE_MEMORY_SCALES, TIMING_SCALE, storageBenchmarkPrimaryRecordCount }} from "@earendil-works/pi-durable/testing";
        const quick = {{ name: "quick", entryCount: 150, taskCount: 70, documentCount: 30 }};
        async function measure(open) {{
            const read = await open();
            const dataset = await seedStorageBenchmark(read, quick);
            const reads = [];
            for (const b of STORAGE_READ_BENCHMARKS) reads.push([b.name, await b.run(read, dataset), b.expected(dataset)]);
            await read.close(context);
            const write = await open();
            await seedStorageWriteBenchmark(write);
            const writes = [];
            for (const b of STORAGE_WRITE_BENCHMARKS) writes.push([b.name, await b.run(write), b.expected]);
            await write.close(context);
            return {{ reads, writes }};
        }}
        export async function run() {{
            const root = temp();
            try {{
                let n = 0;
                return {{
                    memory: await measure(async () => new MemoryStorage()),
                    sqlite: await measure(() => openNodeSqliteStorage(join(root, `bench-${{n++}}.sqlite`))),
                    scales: STORAGE_MEMORY_SCALES.map((s) => s.name).concat(TIMING_SCALE.name),
                    primary: storageBenchmarkPrimaryRecordCount(TIMING_SCALE),
                }};
            }} finally {{ rmSync(root, {{ recursive: true, force: true }}); }}
        }}"#
        ),
    )
    .await;
    for backend in ["memory", "sqlite"] {
        for kind in ["reads", "writes"] {
            let rows = out[backend][kind].as_array().unwrap();
            assert!(!rows.is_empty());
            for row in rows {
                assert_eq!(row[1], row[2], "{backend} {kind} {row}");
            }
        }
    }
    assert_eq!(out["memory"]["reads"].as_array().unwrap().len(), 12);
    assert_eq!(out["scales"], json!(["1k", "10k", "timing"]));
    // 1 root + 1000 entries + 300 tasks + 300 documents + 4 replay + 1 history + 8 * 33 fork records
    assert_eq!(out["primary"], 1 + 1000 + 300 + 300 + 4 + 1 + 8 * 33);
    assert!(calls(&host, "testing.") >= 2);
    rt.shutdown();
}
