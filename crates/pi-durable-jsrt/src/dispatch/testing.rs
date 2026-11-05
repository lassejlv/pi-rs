//! `@earendil-works/pi-durable/testing` (`js/testing/index.js`): the runner-independent
//! conformance cases are `pi_durable::testing`'s; the JS `register*`/`create*` drivers register
//! them with the caller's runner and run each through `testing.*` here, against the
//! environment the caller's provider yields (a JS object, as a native `ExecutionEnv`).

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use pi_durable::testing::{
    EnvConformanceOptions, EnvConformanceProvider, EnvUse, create_env_conformance,
};
use pi_jsrt::JsHandle;

use crate::dispatch::env::js_env;
use crate::errors::{ErrorClass, HostError, HostResult};
use crate::host::{Args, Dispatch, Methods, call_async};
use crate::value::{HostValue, NativeRef};
use pi_json::strict;

/// The testing method group.
pub struct TestingDispatch;

/// The pinned storage contracts as recorded per backend (the traces
/// `pi_durable::testing::create_storage_conformance_for` replays natively): error wording
/// differs per backend, so each has its own recording.
fn storage_fixture(backend: &str) -> HostResult<&'static strict::Value> {
    use std::sync::LazyLock;
    static FIXTURES: LazyLock<Vec<(&'static str, Result<strict::Value, String>)>> =
        LazyLock::new(|| {
            [
                (
                    "memory",
                    include_str!("../../../pi-durable/tests/fixtures/storage.json"),
                ),
                (
                    "jsonl",
                    include_str!("../../../pi-durable/tests/fixtures/storage-jsonl.json"),
                ),
                (
                    "sqlite",
                    include_str!("../../../pi-durable/tests/fixtures/storage-sqlite.json"),
                ),
                (
                    "indexeddb",
                    include_str!("../../../pi-durable/tests/fixtures/storage-indexeddb.json"),
                ),
                (
                    "jsonl-reopen",
                    include_str!("../../../pi-durable/tests/fixtures/storage-jsonl-reopen.json"),
                ),
                (
                    "sqlite-reopen",
                    include_str!("../../../pi-durable/tests/fixtures/storage-sqlite-reopen.json"),
                ),
                (
                    "indexeddb-reopen",
                    include_str!(
                        "../../../pi-durable/tests/fixtures/storage-indexeddb-reopen.json"
                    ),
                ),
            ]
            .into_iter()
            .map(|(name, text)| (name, strict::parse(text)))
            .collect()
        });
    match FIXTURES.iter().find(|(name, _)| *name == backend) {
        Some((_, Ok(fixture))) => Ok(fixture),
        Some((_, Err(e))) => Err(HostError::new(ErrorClass::Error, e.clone())),
        None => Err(HostError::type_error(format!(
            "unknown storage backend {backend}"
        ))),
    }
}

fn storage_cases(backend: &str) -> HostResult<&'static Vec<strict::Value>> {
    storage_fixture(backend)?["cases"]
        .as_array()
        .ok_or_else(|| HostError::new(ErrorClass::Error, "storage fixture without cases"))
}

/// One case body waiting for the provider to call `use(env)`.
struct Pending(Mutex<Option<EnvUse>>);

fn env_options(value: &HostValue) -> EnvConformanceOptions {
    let mut options = EnvConformanceOptions::default();
    if let Some(HostValue::Array(shell)) = value.get("shell") {
        options.shell = shell
            .iter()
            .filter_map(HostValue::as_string)
            .map(|s| s.to_string_lossy())
            .collect();
    }
    if let Some(symlinks) = value.get("symlinks").and_then(HostValue::as_bool) {
        options.symlinks = symlinks;
    }
    if let Some(list_files) = value.get("listFiles").and_then(HostValue::as_bool) {
        options.list_files = list_files;
    }
    options
}

fn durable(error: HostError) -> pi_durable::Error {
    pi_durable::Error::message(error.message().to_string_lossy())
}

/// The provider: `port.provide(withEnv, pending)` calls the JS `withEnv` with a `use` that runs
/// the pending native body (`testing.envUse`).
fn provider(port: JsHandle, with_env: JsHandle) -> EnvConformanceProvider {
    Arc::new(move |body: EnvUse| {
        let (port, with_env) = (port.clone(), with_env.clone());
        Box::pin(async move {
            let pending = Arc::new(Pending(Mutex::new(Some(body))));
            call_async(
                &port,
                Some("provide"),
                vec![
                    HostValue::Handle(with_env),
                    HostValue::Native(NativeRef(pending.clone())),
                ],
            )
            .await
            .map_err(durable)?;
            if pending.0.lock().is_some() {
                return Err(pi_durable::Error::message(
                    "The environment provider did not call its callback",
                ));
            }
            Ok(())
        })
    })
}

fn case_list(options: &HostValue) -> Vec<(&'static str, Option<Duration>)> {
    let unused: EnvConformanceProvider = Arc::new(|_| Box::pin(async { Ok(()) }));
    create_env_conformance(unused, env_options(options))
        .iter()
        .map(|c| (c.name, c.timeout))
        .collect()
}

impl Dispatch for TestingDispatch {
    fn register(&self, m: &mut Methods) {
        // The storage benchmark scales and their primary record counts.
        m.sync("testing.benchmarkScales", |_, _| {
            use pi_durable::testing::{
                STORAGE_MEMORY_SCALES, StorageBenchmarkScale, TIMING_SCALE,
                storage_benchmark_primary_record_count,
            };
            let scale = |s: &StorageBenchmarkScale| {
                HostValue::object([
                    ("name", HostValue::from(s.name)),
                    ("entryCount", HostValue::from(s.entry_count)),
                    ("taskCount", HostValue::from(s.task_count)),
                    ("documentCount", HostValue::from(s.document_count)),
                    (
                        "primaryRecordCount",
                        HostValue::from(storage_benchmark_primary_record_count(s)),
                    ),
                ])
            };
            Ok(HostValue::object([
                (
                    "memory",
                    HostValue::Array(STORAGE_MEMORY_SCALES.iter().map(scale).collect()),
                ),
                ("timing", scale(&TIMING_SCALE)),
            ]))
        });
        // The benchmark lists' names and expected write counts, which the JS drivers must match.
        m.sync("testing.benchmarkDefinitions", |_, _| {
            use pi_durable::testing::{storage_read_benchmarks, storage_write_benchmarks};
            Ok(HostValue::object([
                (
                    "reads",
                    HostValue::Array(
                        storage_read_benchmarks()
                            .iter()
                            .map(|b| HostValue::string(&b.name))
                            .collect(),
                    ),
                ),
                (
                    "writes",
                    HostValue::Array(
                        storage_write_benchmarks()
                            .iter()
                            .map(|b| {
                                HostValue::object([
                                    ("name", HostValue::from(b.name)),
                                    ("expected", HostValue::from(b.expected as f64)),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]))
        });
        // (backend) -> case names, in the reference's order
        m.sync("testing.storageCases", |_, args: Args| {
            Ok(HostValue::Array(
                storage_cases(&args.text(0)?)?
                    .iter()
                    .map(|case| HostValue::from_strict(&case["name"]))
                    .collect(),
            ))
        });
        // (backend, index) -> the case's recorded requests
        m.sync("testing.storageTrace", |_, args: Args| {
            let cases = storage_cases(&args.text(0)?)?;
            let case = cases.get(args.f64(1)? as usize).ok_or_else(|| {
                HostError::new(ErrorClass::RangeError, "unknown conformance case")
            })?;
            Ok(HostValue::from_strict(&case["requests"]))
        });
        // (options) -> [{ name, timeoutMs? }]
        m.sync("testing.envCases", |_, args: Args| {
            Ok(HostValue::Array(
                case_list(args.get(0))
                    .into_iter()
                    .map(|(name, timeout)| match timeout {
                        Some(t) => HostValue::object([
                            ("name", HostValue::from(name)),
                            ("timeoutMs", HostValue::from(t.as_millis() as f64)),
                        ]),
                        None => HostValue::object([("name", HostValue::from(name))]),
                    })
                    .collect(),
            ))
        });
        // (index, options, port, withEnv): run one case through the caller's provider.
        m.asynchronous("testing.envRun", |_, args: Args| async move {
            let index = args.f64(0)? as usize;
            let options = env_options(args.get(1));
            let provider = provider(args.handle(2)?, args.handle(3)?);
            let cases = create_env_conformance(provider, options);
            let case = cases.get(index).ok_or_else(|| {
                HostError::new(ErrorClass::RangeError, "unknown conformance case")
            })?;
            case.run()
                .await
                .map_err(|e| HostError::new(ErrorClass::Error, e.to_string()))?;
            Ok(HostValue::Undefined)
        });
        // (pending, describeEnv(env)): the provider's `use(env)`.
        m.asynchronous("testing.envUse", |_, args: Args| async move {
            let pending = args.native::<Pending>(0)?;
            let body = pending
                .0
                .lock()
                .take()
                .ok_or_else(|| HostError::new(ErrorClass::Error, "use(env) called twice"))?;
            let env = js_env(args.get(1))?
                .ok_or_else(|| HostError::type_error("use(env) needs an environment"))?;
            body(env)
                .await
                .map_err(|e| HostError::new(ErrorClass::Error, e.to_string()))?;
            Ok(HostValue::Undefined)
        });
    }
}
