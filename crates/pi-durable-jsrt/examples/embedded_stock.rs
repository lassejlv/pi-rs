//! Run one pinned, unchanged upstream test file inside pi-jsrt against the embedded packages.
//!
//! ```text
//! cargo run -p pi-durable-jsrt --example embedded_stock -- <pi-mono> --suite=packages/chord/test/output.test.ts \
//!     [--retention=<ms>] [--report=<path>]
//! ```
//!
//! - The reference checkout must be at the pinned commit, and the test file (and the package's
//!   vitest setup files) must be byte-identical to that commit, before and after the run.
//! - `../src/<file>.ts` imports and `@earendil-works/{pi-durable,chord}[/<subpath>]` specifiers
//!   resolve to the embedded modules (`js/`): an export subpath to its public module, any other
//!   upstream source file to `pi-durable:internal/src/<package>/<path>` when the glue provides
//!   one. Everything else under `src/` resolves to a stub that throws, so no upstream
//!   implementation runs.
//! - Durable files run twice, like `npm test`: without `PI_DURABLE_DOCUMENT_RETENTION_MS` and with
//!   it set to `0` (`--retention=<ms>` runs one mode). No name filter; original timeouts.
//! - The report (default `/tmp/pi-durable-embedded-<package>-<file>-report.json`) holds every
//!   case and the native method counters proving execution reached Rust. The exit status is
//!   non-zero when a case fails, an error escapes, nothing ran, or no native method was called.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use parking_lot::Mutex;
use pi_durable_jsrt::{HostOptions, hoist_vitest_mocks, install, modules};
use pi_jsrt::{JsRuntime, RuntimeOptions, VirtualModule};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PIN: &str = "5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
const RETENTION_VAR: &str = "PI_DURABLE_DOCUMENT_RETENTION_MS";
/// Set for the tests' child processes: the reference whose sources node mode routes.
const REFERENCE_VAR: &str = "PI_DURABLE_EMBEDDED_REFERENCE";

struct Package {
    dir: &'static str,
    setup_files: &'static [&'static str],
    retention_modes: bool,
}

const PACKAGES: &[Package] = &[
    Package {
        dir: "packages/durable",
        setup_files: &["test/setup-document-retention.ts"],
        retention_modes: true,
    },
    Package {
        dir: "packages/chord",
        setup_files: &[],
        retention_modes: false,
    },
    // The QuickJS-wasm sandbox cannot run in Boa: `@earendil-works/pi-codemode` is embedded too
    // (`js/pi-codemode/`, over the native `pi-codemode` crate). Both modes, like its stock gate.
    Package {
        dir: "packages/codemode",
        setup_files: &[],
        retention_modes: true,
    },
];

struct Args {
    reference: PathBuf,
    suite: String,
    retention: Option<String>,
    report: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut reference = None;
    let mut suite = None;
    let mut retention = None;
    let mut report = None;
    for arg in std::env::args().skip(1) {
        if let Some(v) = arg.strip_prefix("--suite=") {
            suite = Some(v.to_string());
        } else if let Some(v) = arg.strip_prefix("--retention=") {
            retention = Some(v.to_string());
        } else if let Some(v) = arg.strip_prefix("--report=") {
            report = Some(PathBuf::from(v));
        } else if arg.starts_with("--") {
            fail(&format!("unknown option {arg}"));
        } else {
            reference = Some(PathBuf::from(arg));
        }
    }
    let reference = reference.unwrap_or_else(|| PathBuf::from("../pi-mono"));
    let reference = reference
        .canonicalize()
        .unwrap_or_else(|e| fail(&format!("reference {}: {e}", reference.display())));
    let suite = suite.unwrap_or_else(|| fail("--suite=<packages/.../x.test.ts> is required"));
    Args {
        reference,
        suite,
        retention,
        report,
    }
}

fn fail(message: &str) -> ! {
    eprintln!("embedded_stock: {message}");
    std::process::exit(2)
}

fn git(reference: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .arg("-C")
        .arg(reference)
        .args(args)
        .output()
        .unwrap_or_else(|e| fail(&format!("git: {e}")));
    if !out.status.success() {
        fail(&format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    out.stdout
}

/// The file's bytes, checked against the pinned commit.
fn pinned_bytes(reference: &Path, rel: &str) -> Vec<u8> {
    let bytes = std::fs::read(reference.join(rel)).unwrap_or_else(|e| fail(&format!("{rel}: {e}")));
    let pinned = git(reference, &["show", &format!("{PIN}:{rel}")]);
    if bytes != pinned {
        fail(&format!("{rel} differs from the pinned commit {PIN}"));
    }
    bytes
}

fn package_json(reference: &Path, dir: &str) -> Value {
    let text = std::fs::read_to_string(reference.join(dir).join("package.json"))
        .unwrap_or_else(|e| fail(&format!("{dir}/package.json: {e}")));
    serde_json::from_str(&text).unwrap_or_else(|e| fail(&format!("{dir}/package.json: {e}")))
}

fn ts_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            ts_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "ts") {
            out.push(path);
        }
    }
}

/// Aliases routing upstream sources and package specifiers to embedded modules (or stubs).
fn routing(
    reference: &Path,
    embedded: &BTreeMap<String, &'static str>,
) -> (Vec<(String, String)>, Vec<(String, String)>) {
    let mut aliases = Vec::new();
    let mut stubs = Vec::new();
    fn stub(stubs: &mut Vec<(String, String)>, what: String) -> String {
        let spec = format!("pi-durable-embedded:missing/{what}");
        let message = format!("not embedded: {what} has no pi-durable-jsrt module");
        stubs.push((
            spec.clone(),
            format!(
                "throw new Error({});\n",
                serde_json::to_string(&message).unwrap_or_default()
            ),
        ));
        spec
    }
    for package in PACKAGES {
        let pkg = package_json(reference, package.dir);
        let name = pkg["name"]
            .as_str()
            .unwrap_or_else(|| fail("package.json without a name"))
            .to_string();
        let short = package.dir.trim_start_matches("packages/");
        let src_dir = reference.join(package.dir).join("src");
        let mut exported = BTreeMap::new();
        if let Some(exports) = pkg["exports"].as_object() {
            for (subpath, target) in exports {
                let spec = if subpath == "." {
                    name.clone()
                } else {
                    format!("{name}/{}", subpath.trim_start_matches("./"))
                };
                if let Some(source) = target.get("source").and_then(Value::as_str) {
                    exported.insert(
                        reference
                            .join(package.dir)
                            .join(source.trim_start_matches("./")),
                        spec.clone(),
                    );
                }
                if !embedded.contains_key(&spec) {
                    // A virtual module under the specifier itself: a prefix alias would also
                    // capture the embedded subpaths below it.
                    let message = format!("not embedded: {spec} has no pi-durable-jsrt module");
                    stubs.push((
                        spec.clone(),
                        format!(
                            "throw new Error({});\n",
                            serde_json::to_string(&message).unwrap_or_default()
                        ),
                    ));
                }
            }
        }
        let mut files = Vec::new();
        ts_files(&src_dir, &mut files);
        for file in files {
            let key = file.display().to_string();
            let rel = file
                .strip_prefix(&src_dir)
                .unwrap_or(&file)
                .with_extension("")
                .display()
                .to_string();
            let internal = format!("pi-durable:internal/src/{short}/{rel}");
            let target = match exported.get(&file) {
                Some(spec) if embedded.contains_key(spec) => spec.clone(),
                _ if embedded.contains_key(&internal) => internal,
                _ => stub(&mut stubs, format!("{}/src/{rel}.ts", package.dir)),
            };
            aliases.push((key, target));
        }
    }
    for dir in SOURCE_PACKAGES {
        source_package_aliases(reference, dir, &mut aliases);
    }
    // Substituted dependency: `@silvia-odwyer/photon`'s JS (wasm-bindgen over photon-rs 0.3.3
    // compiled to WebAssembly) is served by the same photon-rs 0.3.3 compiled natively
    // (`pi-durable:internal/photon`); wasmi interprets the WebAssembly encoders far too slowly for
    // the tests' fixture builders. The `.wasm` file itself stays in place for tests that compile it.
    let photon = reference.join("node_modules/@silvia-odwyer/photon/photon_rs.js");
    if photon.is_file() && embedded.contains_key("pi-durable:internal/photon") {
        aliases.push((
            photon.display().to_string(),
            "pi-durable:internal/photon".to_string(),
        ));
    }
    // Test-side dependencies upstream declares but the reference checkout may lack, installed
    // at upstream's pin under this crate (the reference stays read-only): `TEST_DEPENDENCIES`.
    for (spec, entry) in TEST_DEPENDENCIES {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join(entry);
        if file.is_file() {
            aliases.push((spec.to_string(), file.display().to_string()));
        }
    }
    (aliases, stubs)
}

/// `(specifier, entry under this crate)`: test-side dependencies of upstream test files, at
/// upstream's pinned version (`tests/indexeddb/package.json`, `npm ci` there).
const TEST_DEPENDENCIES: &[(&str, &str)] = &[(
    "fake-indexeddb",
    "tests/indexeddb/node_modules/fake-indexeddb/build/esm/index.js",
)];

/// Dependencies the tests load as their pinned TypeScript sources, like the stock gate's
/// `source` condition (their built `dist/` is not part of the pinned tree).
/// `@earendil-works/pi-telemetry` is not here: the embedded packages serve it
/// (`js/pi-telemetry/`, over the native pi-telemetry crate), so no upstream telemetry source runs.
const SOURCE_PACKAGES: &[&str] = &["packages/ai"];

/// Files that never reach the durable runtime, so no native call is expected: `(file, why)`.
const NO_NATIVE: &[(&str, &str)] = &[(
    "packages/durable/test/harness-telemetry-options.test.ts",
    "type-level test: imports only types from pi-durable",
)];

/// Route a source package's specifiers to its pinned `src/` files the way the stock gate's
/// resolver plugin does (`scripts/run-stock-durable-session.mjs`): `<name>` → `src/index.ts`,
/// `<name>/<sub>` → `src/<sub>.ts`, else `src/<sub>/index.ts`, for every subpath (not only the
/// `exports` ones). Exact aliases, one per source file, so nothing outside `src/` is reachable.
fn source_package_aliases(reference: &Path, dir: &str, aliases: &mut Vec<(String, String)>) {
    let pkg = package_json(reference, dir);
    let Some(name) = pkg["name"].as_str() else {
        return;
    };
    let src = reference.join(dir).join("src");
    let mut files = Vec::new();
    ts_files(&src, &mut files);
    files.sort();
    let mut seen = std::collections::BTreeSet::new();
    // `<sub>.ts` wins over `<sub>/index.ts`, as in the plugin's `[path + ".ts", path + "/index.ts"]`.
    for pass in [false, true] {
        for file in &files {
            let rel = file.strip_prefix(&src).unwrap_or(file).with_extension("");
            let rel = rel.display().to_string().replace('\\', "/");
            let is_index = rel == "index" || rel.ends_with("/index");
            let sub = if pass {
                if !is_index {
                    continue;
                }
                rel.trim_end_matches("index")
                    .trim_end_matches('/')
                    .to_string()
            } else {
                rel.clone()
            };
            let spec = if sub.is_empty() {
                name.to_string()
            } else {
                format!("{name}/{sub}")
            };
            if seen.insert(spec.clone()) {
                aliases.push((spec, file.display().to_string()));
            }
        }
    }
}

/// `git status` of the source packages' `src/` trees: empty when every file is the pinned blob.
fn source_tree_changes(reference: &Path) -> String {
    let mut args = vec!["status", "--porcelain", "--untracked-files=all", "--"];
    let dirs: Vec<String> = SOURCE_PACKAGES.iter().map(|d| format!("{d}/src")).collect();
    args.extend(dirs.iter().map(String::as_str));
    String::from_utf8_lossy(&git(reference, &args))
        .trim()
        .to_string()
}

/// Directory the runner's node-mode children write their native counters to (summed per mode).
const COUNTERS_VAR: &str = "PI_DURABLE_EMBEDDED_COUNTERS_DIR";

/// Sum the counter files children wrote into `dir`.
fn child_counters(dir: &Path) -> (BTreeMap<String, u64>, usize) {
    let mut total = BTreeMap::new();
    let mut children = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        children += 1;
        for (method, count) in map {
            *total.entry(method).or_insert(0) += count.as_u64().unwrap_or(0);
        }
    }
    (total, children)
}

struct ModeResult {
    retention: Option<String>,
    report: Value,
    uncaught: Vec<String>,
    native: BTreeMap<String, u64>,
    /// Native calls made by node-mode child processes (`process.execPath …`), and their count.
    child_native: BTreeMap<String, u64>,
    children: usize,
    load_error: Option<String>,
}

fn run_mode(args: &Args, package: &Package, retention: Option<String>) -> ModeResult {
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap_or_else(|e| fail(&e.to_string()));
    let package_dir = args.reference.join(package.dir);
    let embedded: BTreeMap<String, &'static str> = modules(true).into_iter().collect();
    let (aliases, stubs) = routing(&args.reference, &embedded);
    let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
    env.remove(RETENTION_VAR);
    env.insert(
        REFERENCE_VAR.to_string(),
        args.reference.display().to_string(),
    );
    if let Some(ms) = &retention {
        env.insert(RETENTION_VAR.to_string(), ms.clone());
    }
    let counters_dir = tempfile::tempdir().unwrap_or_else(|e| fail(&e.to_string()));
    env.insert(
        COUNTERS_VAR.to_string(),
        counters_dir.path().display().to_string(),
    );
    let uncaught = Arc::new(Mutex::new(Vec::new()));
    let sink = uncaught.clone();
    let options = RuntimeOptions {
        cwd: Some(package_dir.clone()),
        env: Some(env),
        argv: vec!["node".into(), "vitest".into()],
        tokio: Some(tokio.handle().clone()),
        aliases: aliases.into_iter().collect(),
        virtual_modules: stubs
            .into_iter()
            .map(|(spec, src)| (spec, VirtualModule::EsmJs(src)))
            .collect(),
        on_uncaught: Some(Arc::new(move |kind: &str, error: &pi_jsrt::Error| {
            let text = match error {
                pi_jsrt::Error::Js(e) => e.stack.clone().unwrap_or_else(|| e.to_string()),
                other => other.to_string(),
            };
            // Also live, so a trace (`PI_DURABLE_NATIVE_TRACE=1`) shows where it happened.
            if std::env::var_os("PI_DURABLE_NATIVE_TRACE").is_some() {
                eprintln!("[uncaught] {kind}: {text}");
            }
            sink.lock().push(format!("{kind}: {text}"));
        })),
        expose_gc: true,
        timer_control: true,
        ..Default::default()
    };
    let test_path = args.reference.join(&args.suite);
    let setup: Vec<PathBuf> = package
        .setup_files
        .iter()
        .map(|f| package_dir.join(f))
        .collect();
    let suite = args.suite.clone();
    let outcome = tokio.block_on(async move {
        let rt = JsRuntime::new(options).map_err(|e| e.to_string())?;
        let host = install(&rt, HostOptions { vitest: true, ..Default::default() }).await.map_err(|e| e.to_string())?;
        let vitest = rt.import("vitest").await.map_err(|e| e.to_string())?;
        let mut load_error = None;
        for file in setup.iter().chain(std::iter::once(&test_path)) {
            // vitest's mock hoisting, served in place of the (unchanged) file on disk.
            let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            if let Some(hoisted) = hoist_vitest_mocks(&text, &file.display().to_string())? {
                rt.set_file_source(file.clone(), hoisted);
            }
            if let Err(e) = rt.import(file.display().to_string()).await {
                load_error = Some(format!("{}: {e}", file.display()));
                break;
            }
        }
        let runner = vitest.get_handle("__runner").await.map_err(|e| e.to_string())?;
        let report = match load_error {
            Some(_) => {
                let mocks = runner.call_method("mocks", vec![]).await.map_err(|e| e.to_string())?;
                json!({
                    "numTotalTests": 0, "numPassedTests": 0, "numFailedTests": 0, "success": false, "testResults": [],
                    "appliedMocks": mocks["appliedMocks"], "unappliedMocks": mocks["unappliedMocks"],
                })
            }
            None => runner.call_method("run", vec![json!({ "file": suite })]).await.map_err(|e| e.to_string())?,
        };
        let native = host.counters();
        rt.shutdown();
        Ok::<_, String>((report, native, load_error))
    });
    tokio.shutdown_background();
    let (report, native, load_error) =
        outcome.unwrap_or_else(|e| (Value::Null, BTreeMap::new(), Some(e)));
    let uncaught = uncaught.lock().clone();
    let (child_native, children) = child_counters(counters_dir.path());
    ModeResult {
        retention,
        report,
        uncaught,
        native,
        child_native,
        children,
        load_error,
    }
}

/// Node mode. This binary is the tests' `process.execPath`, so upstream tests that start
/// `process.execPath -e <script>` (or `<exe> script.js`) run here like `node` would: in pi-jsrt,
/// with the embedded packages installed and upstream sources routed as for the test file.
fn node_mode() -> Option<i32> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(pi_jsrt::node::NODE_MARKER) {
        args.remove(0);
    } else {
        let first = args.first()?;
        let ours = ["--suite=", "--retention=", "--report="]
            .iter()
            .any(|p| first.starts_with(p));
        let script = [".js", ".mjs", ".cjs", ".ts", ".mts"]
            .iter()
            .any(|e| first.ends_with(e));
        if ours || !(first.starts_with('-') || script) {
            return None;
        }
    }
    let reference = std::env::var_os(REFERENCE_VAR).map(PathBuf::from);
    let code = pi_jsrt::node::run_with(
        args,
        |options| {
            if let Some(reference) = &reference {
                let embedded: BTreeMap<String, &'static str> = modules(false).into_iter().collect();
                let (aliases, stubs) = routing(reference, &embedded);
                options.aliases = aliases.into_iter().collect();
                options.virtual_modules = stubs
                    .into_iter()
                    .map(|(spec, src)| (spec, VirtualModule::EsmJs(src)))
                    .collect();
            }
        },
        |rt| async move {
            let host = install(&rt, HostOptions::default())
                .await
                .map_err(|e| e.to_string())?;
            *CHILD_HOST.lock() = Some(host);
            Ok(())
        },
    );
    // Report this child's native calls to the parent run (summed into its mode's counters).
    if let (Some(dir), Some(host)) = (std::env::var_os(COUNTERS_VAR), CHILD_HOST.lock().take()) {
        let counters = serde_json::to_string(&host.counters()).unwrap_or_default();
        let _ = std::fs::write(
            PathBuf::from(dir).join(format!("{}.json", std::process::id())),
            counters,
        );
    }
    Some(code)
}

/// The host a node-mode child installed, read back for its counters after the run.
static CHILD_HOST: Mutex<Option<pi_durable_jsrt::DurableHost>> = Mutex::new(None);

fn main() {
    if let Some(code) = node_mode() {
        std::process::exit(code);
    }
    let args = parse_args();
    let head = String::from_utf8_lossy(&git(&args.reference, &["rev-parse", "HEAD"]))
        .trim()
        .to_string();
    if head != PIN {
        fail(&format!("reference HEAD {head} is not the pinned {PIN}"));
    }
    let package = PACKAGES
        .iter()
        .find(|p| args.suite.starts_with(&format!("{}/", p.dir)))
        .unwrap_or_else(|| {
            fail(&format!(
                "{} is not in {}",
                args.suite,
                PACKAGES
                    .iter()
                    .map(|p| p.dir)
                    .collect::<Vec<_>>()
                    .join(" or ")
            ))
        });
    let source_changes = source_tree_changes(&args.reference);
    if !source_changes.is_empty() {
        fail(&format!(
            "aliased source packages differ from the pinned commit:\n{source_changes}"
        ));
    }
    let before = pinned_bytes(&args.reference, &args.suite);
    let setup_before: Vec<Vec<u8>> = package
        .setup_files
        .iter()
        .map(|f| pinned_bytes(&args.reference, &format!("{}/{f}", package.dir)))
        .collect();

    let modes: Vec<Option<String>> = match (&args.retention, package.retention_modes) {
        (Some(ms), _) => vec![Some(ms.clone())],
        (None, true) => vec![None, Some("0".to_string())],
        (None, false) => vec![None],
    };
    let mut ok = true;
    let mut results = Vec::new();
    for retention in modes {
        let r = run_mode(&args, package, retention);
        let mode = r.retention.clone().unwrap_or_else(|| "default".to_string());
        let total = r.report["numTotalTests"].as_u64().unwrap_or(0);
        let passed = r.report["numPassedTests"].as_u64().unwrap_or(0);
        let failed = r.report["numFailedTests"].as_u64().unwrap_or(0);
        let skipped = r.report["numPendingTests"].as_u64().unwrap_or(0)
            + r.report["numTodoTests"].as_u64().unwrap_or(0);
        let calls: u64 = r.native.values().sum::<u64>() + r.child_native.values().sum::<u64>();
        let file_message = r.report["testResults"][0]["message"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let unapplied = r.report["unappliedMocks"]
            .as_array()
            .is_some_and(|m| !m.is_empty());
        let mode_ok = r.load_error.is_none()
            && r.report["success"].as_bool() == Some(true)
            && r.uncaught.is_empty()
            && total > 0
            && passed + skipped == total
            && (calls > 0 || NO_NATIVE.iter().any(|(file, _)| *file == args.suite))
            && !unapplied;
        if let Some(e) = &r.load_error {
            eprintln!("load failure: {e}");
        }
        for m in r.report["appliedMocks"].as_array().into_iter().flatten() {
            println!("vi.mock({m}) applied");
        }
        if !file_message.is_empty() {
            eprintln!("file errors:\n{file_message}");
        }
        for test in r.report["testResults"][0]["assertionResults"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if test["status"] == "failed" {
                eprintln!("FAIL {}", test["fullName"].as_str().unwrap_or(""));
                for m in test["failureMessages"].as_array().into_iter().flatten() {
                    eprintln!("    {}", m.as_str().unwrap_or("").replace('\n', "\n    "));
                }
            }
        }
        for u in &r.uncaught {
            eprintln!("{u}");
        }
        for m in r.report["unappliedMocks"].as_array().into_iter().flatten() {
            eprintln!("vi.mock({m}) was registered but never applied (not a top-level call)");
        }

        println!(
            "Embedded {} retention={mode}: {passed}/{total} passed, {failed} failed, {skipped} skipped, {} uncaught; native calls={calls} ({} methods){}",
            args.suite,
            r.uncaught.len(),
            r.native
                .keys()
                .chain(r.child_native.keys())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            if mode_ok { "" } else { " FAILED" }
        );
        ok &= mode_ok;
        results.push(json!({
            "retention": r.retention,
            "passed": passed,
            "failed": failed,
            "skipped": skipped,
            "total": total,
            "uncaught": r.uncaught,
            "loadError": r.load_error,
            "appliedMocks": r.report["appliedMocks"],
            "unappliedMocks": r.report["unappliedMocks"],
            "native": { "calls": calls, "methods": r.native, "children": r.children, "childMethods": r.child_native },
            "tests": r.report["testResults"][0]["assertionResults"].as_array().cloned().unwrap_or_default().into_iter().map(|t| json!({
                "name": t["fullName"], "status": t["status"], "durationMs": t["duration"], "failureMessages": t["failureMessages"],
            })).collect::<Vec<_>>(),
        }));
    }

    if source_tree_changes(&args.reference) != source_changes {
        fail("aliased source packages changed during the run");
    }
    let after = std::fs::read(args.reference.join(&args.suite)).unwrap_or_default();
    if after != before {
        fail("pinned test file changed during the run");
    }
    for (f, bytes) in package.setup_files.iter().zip(&setup_before) {
        if std::fs::read(args.reference.join(package.dir).join(f)).unwrap_or_default() != *bytes {
            fail(&format!("pinned setup file {f} changed during the run"));
        }
    }
    let file_id = args
        .suite
        .trim_start_matches("packages/")
        .trim_end_matches(".ts")
        .trim_end_matches(".test")
        .replace("/test/", "-")
        .replace('/', "-");
    let report_path = args.report.clone().unwrap_or_else(|| {
        PathBuf::from(format!("/tmp/pi-durable-embedded-{file_id}-report.json"))
    });
    let report = json!({
        "reference": PIN,
        "test": args.suite,
        "sha256": format!("{:x}", Sha256::digest(&before)),
        "selection": "complete file without name filter",
        "runtime": "pi-jsrt (Boa) with pi-durable-jsrt embedded modules and the vitest shim",
        "success": ok,
        "sourceAliases": {
            "packages": SOURCE_PACKAGES,
            "rule": "<name> -> src/index.ts; <name>/<sub> -> src/<sub>.ts else src/<sub>/index.ts (stock resolver plugin)",
            "pinnedClean": true,
        },
        "testDependencies": TEST_DEPENDENCIES
            .iter()
            .map(|(spec, entry)| json!({ "specifier": spec, "entry": format!("crates/pi-durable-jsrt/{entry}"), "pin": "tests/indexeddb/package-lock.json" }))
            .collect::<Vec<_>>(),
        "modes": results,
    });
    if let Err(e) = std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    ) {
        eprintln!(
            "embedded_stock: cannot write {}: {e}",
            report_path.display()
        );
        ok = false;
    } else {
        println!("report: {}", report_path.display());
    }
    std::process::exit(if ok { 0 } else { 1 });
}
