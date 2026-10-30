//! Shared helpers for the plain (`harness = false`) benchmarks.
//!
//! Set `PI_DURABLE_BENCH_QUICK=1` for the small scales; `PI_DURABLE_BENCH_SIZES=a,b,c` overrides a benchmark's
//! primary scale list in either mode.
#![allow(dead_code)]
use parking_lot::Mutex;
use pi_ai::{
    models::Models,
    providers::faux::{
        FauxMessageOptions, FauxProviderHandle, FauxResponseStep, RegisterFauxProviderOptions,
        faux_assistant_message, faux_provider, faux_text, faux_tool_call,
    },
};
use pi_durable::{
    harness::declaration::ToolDeclaration,
    storage::{Adopt, StorageFuture},
    *,
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

// ─── Allocation accounting: the native counterpart of upstream heap snapshots ───

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
/// Counts live heap bytes. Install with `#[global_allocator] static A: support::Counting = support::Counting;`.
pub struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let now = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, size) };
        if !moved.is_null() {
            if size >= layout.size() {
                let now =
                    LIVE.fetch_add(size - layout.size(), Ordering::Relaxed) + size - layout.size();
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(layout.size() - size, Ordering::Relaxed);
            }
        }
        moved
    }
}
/// Live heap bytes now.
pub fn live_bytes() -> usize {
    LIVE.load(Ordering::Relaxed)
}
/// Peak live heap bytes since the last `reset_peak`.
pub fn peak_bytes() -> usize {
    PEAK.load(Ordering::Relaxed)
}
pub fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

// ─── Scale selection ───

pub fn quick() -> bool {
    std::env::var("PI_DURABLE_BENCH_QUICK").is_ok_and(|value| !value.is_empty() && value != "0")
}
/// `PI_DURABLE_BENCH_SIZES`, else the quick or full list.
pub fn sizes(full: &[usize], quick_sizes: &[usize]) -> Vec<usize> {
    if let Ok(list) = std::env::var("PI_DURABLE_BENCH_SIZES") {
        let parsed: Vec<usize> = list
            .split(',')
            .filter_map(|part| part.trim().parse().ok())
            .collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }
    if quick() { quick_sizes } else { full }.to_vec()
}
pub fn mib(bytes: usize) -> String {
    format!("{:.2} MiB", bytes as f64 / 1024.0 / 1024.0)
}
pub fn kib(bytes: usize) -> String {
    format!("{:.1} KiB", bytes as f64 / 1024.0)
}
pub fn dir_size(path: &Path) -> usize {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len() as usize;
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| dir_size(&entry.path()))
        .sum()
}
/// Size of a SQLite database together with its WAL and shared-memory files.
pub fn sqlite_size(path: &Path) -> usize {
    ["", "-wal", "-shm"]
        .iter()
        .map(|suffix| dir_size(Path::new(&format!("{}{suffix}", path.display()))))
        .sum()
}
pub fn bench_err(error: impl std::fmt::Display) -> Error {
    Error::message(error.to_string())
}

// ─── JSON and registry helpers ───

pub type Reg = Registry<NativeTool, NativeSection>;
pub type Ext = Extension<NativeTool, NativeSection>;
pub fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).expect("valid JSON")
}
pub fn object(text: &str) -> JsonObject {
    JsonObject::try_from_value(json(text)).expect("JSON object")
}
pub fn sj(value: &JsonValue) -> serde_json::Value {
    serde_json::from_str(&value.to_string()).expect("serde JSON")
}
pub fn ctx() -> Context {
    Context::default()
}
pub fn registry() -> Reg {
    create_registry().expect("registry")
}
pub fn install(registry: &Reg, extension: Ext) {
    registry.install(Arc::new(extension)).expect("install");
}
pub fn tool<F>(name: &str, description: &str, parameters: &str, execute: F) -> NativeTool
where
    F: Fn(JsonObject, ToolExecutionApi, Context) -> StorageFuture<ToolExecutionResult>
        + Send
        + Sync
        + 'static,
{
    NativeTool::new(
        ToolDeclaration::new(name, description, json(parameters)),
        Arc::new(execute),
    )
}
pub fn text_result(text: &str) -> ToolExecutionResult {
    let block = object(&serde_json::json!({"type": "text", "text": text}).to_string());
    ToolExecutionResult {
        output: Some(Arc::new(vec![Arc::new(block)])),
        ..Default::default()
    }
}
pub fn faux(options: RegisterFauxProviderOptions) -> (Models, FauxProviderHandle) {
    let models = Models::default();
    let faux = faux_provider(options);
    models.set_provider(faux.provider.clone());
    (models, faux)
}
pub fn fast() -> RegisterFauxProviderOptions {
    RegisterFauxProviderOptions {
        tokens_per_second: Some(f64::INFINITY),
        ..Default::default()
    }
}
pub fn call(name: &str, args: serde_json::Value, id: &str) -> FauxResponseStep {
    let args = args.as_object().cloned().unwrap_or_default();
    faux_assistant_message(
        vec![faux_tool_call(name, args, Some(id))],
        FauxMessageOptions {
            stop_reason: Some(pi_ai::StopReason::ToolUse),
            ..Default::default()
        },
    )
    .into()
}
pub fn answer(text: &str) -> FauxResponseStep {
    faux_assistant_message(vec![faux_text(text)], Default::default()).into()
}
pub fn root_options() -> ConversationOptions {
    ConversationOptions {
        agent: Some(object(
            r#"{"model":{"provider":"faux","modelId":"faux-1"}}"#,
        )),
        ..Default::default()
    }
}
/// Fail unless the newest tool result is a success. A run that answered "done" may still have failed tools.
pub async fn assert_tool_succeeded(root: &Conversation, what: &str) -> Result<()> {
    let page = root.entries(Default::default(), 10, None, &ctx()).await?;
    let entry = page
        .items
        .iter()
        .find(|entry| entry.kind.equals_str("pi.tool-result"))
        .ok_or_else(|| Error::message(format!("{what}: no tool result")))?;
    let message = sj(entry
        .model
        .as_ref()
        .and_then(|model| model.first())
        .expect("model message"));
    if message["isError"] == true {
        return Err(Error::message(format!("{what}: a tool call failed")));
    }
    Ok(())
}

// ─── A storage that counts commits and the bytes of the writes it receives ───

#[derive(Default)]
pub struct Tally {
    pub commits: AtomicU64,
    /// Logical bytes (JSON of the writes) by write type.
    pub written: Mutex<BTreeMap<String, usize>>,
    pub reads: AtomicU64,
    /// Milliseconds each `commit` took.
    pub latencies: Mutex<Vec<f64>>,
}
impl Tally {
    pub fn snapshot(&self) -> (u64, BTreeMap<String, usize>) {
        (
            self.commits.load(Ordering::SeqCst),
            self.written.lock().clone(),
        )
    }
}
pub fn delta(
    before: &BTreeMap<String, usize>,
    after: &BTreeMap<String, usize>,
    kind: &str,
) -> usize {
    after.get(kind).copied().unwrap_or(0) - before.get(kind).copied().unwrap_or(0)
}
pub fn total(written: &BTreeMap<String, usize>) -> usize {
    written.values().sum()
}
pub struct Counted {
    pub inner: Arc<dyn Storage>,
    pub tally: Arc<Tally>,
}
impl Counted {
    pub fn wrap(inner: Arc<dyn Storage>) -> (Arc<Self>, Arc<Tally>) {
        let tally = Arc::new(Tally::default());
        (
            Arc::new(Self {
                inner,
                tally: tally.clone(),
            }),
            tally,
        )
    }
}
macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty);* $(;)?) => {$(
        fn $name(&self, $($arg: $ty),*) -> StorageFuture<$ret> {
            self.inner.$name($($arg),*)
        }
    )*};
}
impl Storage for Counted {
    fn commit(
        &self,
        writes: &[StorageWrite],
        adopt: Adopt,
        context: &Context,
    ) -> StorageFuture<Seq> {
        self.tally.commits.fetch_add(1, Ordering::SeqCst);
        {
            let mut written = self.tally.written.lock();
            for write in writes {
                let value = serde_json::to_value(write).unwrap_or_default();
                let kind = value["type"].as_str().unwrap_or("?").to_string();
                *written.entry(kind).or_default() += value.to_string().len();
            }
        }
        let commit = self.inner.commit(writes, adopt, context);
        let tally = self.tally.clone();
        let kinds: Vec<String> = writes
            .iter()
            .map(|write| {
                serde_json::to_value(write).unwrap_or_default()["type"]
                    .as_str()
                    .unwrap_or("?")
                    .to_string()
            })
            .collect();
        Box::pin(async move {
            let start = std::time::Instant::now();
            let result = commit.await;
            if std::env::var("PI_DURABLE_BENCH_TRACE").is_ok() {
                static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                let origin = ORIGIN.get_or_init(std::time::Instant::now);
                eprintln!(
                    "t={:>7.0} ms commit {:>6.1} ms {:?}",
                    start.duration_since(*origin).as_secs_f64() * 1000.0,
                    start.elapsed().as_secs_f64() * 1000.0,
                    kinds
                );
            }
            tally
                .latencies
                .lock()
                .push(start.elapsed().as_secs_f64() * 1000.0);
            result
        })
    }
    fn task(&self, id: TaskId, context: &Context) -> StorageFuture<Option<TaskRecord>> {
        self.tally.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.task(id, context)
    }
    fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> StorageFuture<Option<ConversationRecord>> {
        self.tally.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.conversation(id, context)
    }
    forward! {
        mint_id() -> i64;
        scan_conversations(query: &ConversationQuery, limit: usize, cursor: Option<&Cursor>, context: &Context) -> Page<ConversationRecord>;
        entry(id: EntryId, context: &Context) -> Option<storage::StoredEntry>;
        entry_in(conversation: ConversationId, id: EntryId, context: &Context) -> Option<storage::StoredEntry>;
        find_latest_head_marker(conversation: ConversationId, at_or_before: Option<EntryId>, context: &Context) -> Option<EntryRecord>;
        scan_entries(query: &EntryQuery, limit: usize, cursor: Option<&Cursor>, context: &Context) -> Page<EntryRecord>;
        scan_tasks(query: &TaskQuery, limit: usize, cursor: Option<&Cursor>, context: &Context) -> Page<TaskRecord>;
        submission(id: SubmissionId, context: &Context) -> Option<SubmissionRecord>;
        scan_submissions(query: &SubmissionQuery, limit: usize, cursor: Option<&Cursor>, context: &Context) -> Page<SubmissionRecord>;
        submission_by_request(conversation: ConversationId, request: &JsonString, context: &Context) -> Option<SubmissionRecord>;
        find_document(address: &DocumentAddress, at: DocumentPoint, context: &Context) -> Option<DocumentRecord>;
        document(id: DocumentId, at: DocumentPoint, context: &Context) -> Option<StoredDocument>;
        scan_documents(query: &DocumentQuery, limit: usize, cursor: Option<&Cursor>, context: &Context) -> Page<DocumentRecord>;
        close(context: &Context) -> ()
    }
}
pub fn time<T>(f: impl FnOnce() -> T) -> (T, std::time::Duration) {
    let start = std::time::Instant::now();
    let value = f();
    (value, start.elapsed())
}
