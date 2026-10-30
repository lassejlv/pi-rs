//! Port of `packages/coding-agent/src/utils/fs-watch.ts`, plus the `fs.watchFile` polling that
//! TS callers use as a fallback.
//!
//! Node's event-driven `fs.watch` is backed by the `notify` crate (inotify on Linux, FSEvents on
//! macOS, kqueue on the BSDs, ReadDirectoryChangesW on Windows). Events follow Node's libuv
//! mapping: content or attribute changes are `"change"`, entries appearing, disappearing or being
//! renamed are `"rename"`; the file name is the entry name relative to a watched directory, or the
//! base name when the watched path itself changes. Like Node, the watcher does not debounce:
//! callers debounce (theme hot reload, footer git branch refresh).
//!
//! [`watch_file`] ports `fs.watchFile`: a stat-polling watcher that calls the listener with the
//! current and previous stats whenever they differ (zeroed stats while the file is missing).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{RecursiveMode, Watcher};
use parking_lot::Mutex;

pub const FS_WATCH_RETRY_DELAY_MS: i64 = 5000;

/// Node `WatchEventType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEventType {
    Rename,
    Change,
}

impl WatchEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::Change => "change",
        }
    }
}

/// `WatchListener<string>`: `(eventType, filename)`.
pub type WatchListener = Arc<dyn Fn(WatchEventType, Option<String>) + Send + Sync>;

/// The `"error"` listener of a watcher.
pub type WatchErrorListener = Arc<dyn Fn() + Send + Sync>;

struct WatcherState {
    closed: AtomicBool,
    listener: WatchListener,
    on_error: WatchErrorListener,
    backend: Mutex<Option<notify::RecommendedWatcher>>,
}

impl WatcherState {
    fn emit(&self, kind: WatchEventType, filename: Option<String>) {
        if !self.closed.load(Ordering::SeqCst) {
            (self.listener)(kind, filename);
        }
    }

    fn emit_error(&self) {
        if !self.closed.load(Ordering::SeqCst) {
            (self.on_error)();
        }
    }
}

/// A running watcher (`FSWatcher`). Cloning shares the same watcher.
#[derive(Clone)]
pub struct FsWatcher {
    state: Arc<WatcherState>,
}

impl FsWatcher {
    /// Stop watching. Idempotent; no listener runs after it returns.
    pub fn close(&self) {
        self.state.closed.store(true, Ordering::SeqCst);
        // Dropping the backend stops its event thread (it does not join, so closing from inside
        // a listener is fine).
        let backend = self.state.backend.lock().take();
        drop(backend);
    }

    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::SeqCst)
    }

    /// `watcher.emit("change", eventType, filename)`: deliver an event to the listener, as the
    /// TS tests do on the `FSWatcher` event emitter.
    #[doc(hidden)]
    pub fn emit_event(&self, kind: WatchEventType, filename: Option<&str>) {
        self.state.emit(kind, filename.map(str::to_string));
    }

    /// `watcher.emit("error", error)`: deliver an error to the error listener.
    #[doc(hidden)]
    pub fn emit_error(&self) {
        self.state.emit_error();
    }

    /// Whether two handles refer to the same watcher.
    pub fn ptr_eq(&self, other: &FsWatcher) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}

impl std::fmt::Debug for FsWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsWatcher").field("closed", &self.is_closed()).finish()
    }
}

pub fn close_watcher(watcher: Option<&FsWatcher>) {
    if let Some(watcher) = watcher {
        watcher.close();
    }
}

fn base_name(path: &Path) -> Option<String> {
    path.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// The Node event type for a notify event, or `None` for events Node does not report
/// (opens, closes, reads).
fn event_type(kind: &EventKind) -> Option<WatchEventType> {
    match kind {
        EventKind::Access(_) => None,
        // inotify reports a paired rename as From + To and then Both; libuv reports the two halves.
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => None,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => {
            Some(WatchEventType::Rename)
        }
        EventKind::Modify(_) | EventKind::Any | EventKind::Other => Some(WatchEventType::Change),
    }
}

/// The file name Node reports for an event path under the watched `root`.
fn event_file_name(root: &Path, path: &Path) -> Option<String> {
    if path == root {
        return base_name(root);
    }
    match path.strip_prefix(root) {
        Ok(relative) if !relative.as_os_str().is_empty() => Some(relative.to_string_lossy().into_owned()),
        _ => base_name(path),
    }
}

/// Start watching `path` (`fs.watch(path, listener)` with an `"error"` handler). When the path
/// cannot be watched (for example it does not exist), `on_error` runs and `None` is returned.
/// Errors reported by the backend later also call `on_error`.
pub fn watch_with_error_handler(
    path: &str,
    listener: WatchListener,
    on_error: WatchErrorListener,
) -> Option<FsWatcher> {
    let state = Arc::new(WatcherState {
        closed: AtomicBool::new(false),
        listener,
        on_error: on_error.clone(),
        backend: Mutex::new(None),
    });
    let root = PathBuf::from(path);
    // Event paths are absolute; match them against the absolute watched path.
    let root_abs = std::path::absolute(&root).unwrap_or_else(|_| root.clone());
    let root_canonical = std::fs::canonicalize(&root).ok();
    let weak = Arc::downgrade(&state);
    let handler = move |result: notify::Result<notify::Event>| {
        let Some(state) = weak.upgrade() else { return };
        match result {
            Ok(event) => {
                let Some(kind) = event_type(&event.kind) else { return };
                if event.paths.is_empty() {
                    state.emit(kind, None);
                    return;
                }
                for event_path in &event.paths {
                    let name = if event_path.starts_with(&root_abs) {
                        event_file_name(&root_abs, event_path)
                    } else if let Some(canonical) = root_canonical.as_ref().filter(|c| event_path.starts_with(c)) {
                        event_file_name(canonical, event_path)
                    } else {
                        base_name(event_path)
                    };
                    state.emit(kind, name);
                }
            }
            Err(_) => state.emit_error(),
        }
    };
    let mut backend = match notify::recommended_watcher(handler) {
        Ok(backend) => backend,
        Err(_) => {
            on_error();
            return None;
        }
    };
    if backend.watch(&root, RecursiveMode::NonRecursive).is_err() {
        on_error();
        return None;
    }
    *state.backend.lock() = Some(backend);
    Some(FsWatcher { state })
}

// ============================================================================
// fs.watchFile
// ============================================================================

/// The `fs.Stats` fields `fs.watchFile` listeners compare. All zero while the file is missing.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WatchFileStats {
    pub mtime_ms: f64,
    pub ctime_ms: f64,
    pub size: u64,
    pub ino: u64,
    pub mode: u32,
}

impl WatchFileStats {
    fn read(path: &Path) -> Self {
        let Ok(meta) = std::fs::metadata(path) else {
            return Self::default();
        };
        let ms = |t: std::io::Result<std::time::SystemTime>| {
            t.ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64() * 1000.0)
                .unwrap_or(0.0)
        };
        #[cfg(unix)]
        let (ctime_ms, ino, mode) = {
            use std::os::unix::fs::MetadataExt;
            (meta.ctime() as f64 * 1000.0 + meta.ctime_nsec() as f64 / 1e6, meta.ino(), meta.mode())
        };
        #[cfg(not(unix))]
        let (ctime_ms, ino, mode) = (ms(meta.created()), 0, 0);
        Self { mtime_ms: ms(meta.modified()), ctime_ms, size: meta.len(), ino, mode }
    }
}

/// `(current, previous)` stats listener of [`watch_file`].
pub type WatchFileListener = Arc<dyn Fn(&WatchFileStats, &WatchFileStats) + Send + Sync>;

/// A running `fs.watchFile` poller; [`FileWatcher::close`] is `fs.unwatchFile`.
#[derive(Clone)]
pub struct FileWatcher {
    closed: Arc<AtomicBool>,
}

impl FileWatcher {
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for FileWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileWatcher").field("closed", &self.is_closed()).finish()
    }
}

/// `fs.watchFile(path, { interval }, listener)`: stat `path` every `interval_ms` and call the
/// listener when the stats changed.
pub fn watch_file(path: &str, interval_ms: u64, listener: WatchFileListener) -> FileWatcher {
    let watcher = FileWatcher { closed: Arc::new(AtomicBool::new(false)) };
    let closed = watcher.closed.clone();
    let path = PathBuf::from(path);
    let mut previous = WatchFileStats::read(&path);
    let spawned = std::thread::Builder::new().name("pi-watch-file".into()).spawn(crate::utils::uncaught::supervise_fn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(interval_ms.max(1)));
            if closed.load(Ordering::SeqCst) {
                return;
            }
            let current = WatchFileStats::read(&path);
            if current != previous {
                let before = std::mem::replace(&mut previous, current);
                if !closed.load(Ordering::SeqCst) {
                    listener(&current, &before);
                }
            }
        }
    }));
    if spawned.is_err() {
        watcher.close();
    }
    watcher
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    type Events = Arc<Mutex<Vec<(WatchEventType, Option<String>)>>>;

    fn wait_until(condition: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    fn recorder() -> (Events, WatchListener) {
        let events: Events = Arc::default();
        let sink = events.clone();
        (events, Arc::new(move |kind, name| sink.lock().push((kind, name))))
    }

    #[test]
    fn missing_path_calls_on_error() {
        let errors = Arc::new(Mutex::new(0));
        let e = errors.clone();
        let watcher = watch_with_error_handler(
            "/definitely/not/here/pi-watch",
            Arc::new(|_, _| {}),
            Arc::new(move || *e.lock() += 1),
        );
        assert!(watcher.is_none());
        assert_eq!(*errors.lock(), 1);
        close_watcher(None);
    }

    #[test]
    fn reports_directory_entry_events_with_node_types() {
        let dir = tempfile::tempdir().unwrap();
        let (events, listener) = recorder();
        let watcher = watch_with_error_handler(dir.path().to_str().unwrap(), listener, Arc::new(|| {})).unwrap();

        std::fs::write(dir.path().join("a.json"), "1").unwrap();
        assert!(wait_until(|| events.lock().iter().any(|e| e.0 == WatchEventType::Change)));
        {
            let events = events.lock();
            assert_eq!(events[0], (WatchEventType::Rename, Some("a.json".to_string())));
            assert!(events.iter().all(|(_, name)| name.as_deref() == Some("a.json")));
        }

        events.lock().clear();
        std::fs::rename(dir.path().join("a.json"), dir.path().join("b.json")).unwrap();
        // FSEvents coalesces flags per path, so a half may repeat or arrive late: require both halves.
        let renamed = |name: &str| events.lock().contains(&(WatchEventType::Rename, Some(name.to_string())));
        assert!(wait_until(|| renamed("a.json") && renamed("b.json")));

        watcher.close();
        events.lock().clear();
        std::fs::write(dir.path().join("c.json"), "1").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(events.lock().is_empty(), "no events after close");
    }

    #[test]
    fn watching_a_file_reports_its_base_name() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tables.list");
        std::fs::write(&file, "0\n").unwrap();
        let (events, listener) = recorder();
        let watcher = watch_with_error_handler(file.to_str().unwrap(), listener, Arc::new(|| {})).unwrap();
        std::fs::write(&file, "1\n").unwrap();
        // FSEvents keeps the Created flag of the file written just before the watch started, so the
        // first event can be a rename; a change for the base name must still arrive.
        let changed = || events.lock().contains(&(WatchEventType::Change, Some("tables.list".to_string())));
        assert!(wait_until(changed));
        assert!(events.lock().iter().all(|(_, name)| name.as_deref() == Some("tables.list")));
        watcher.close();
    }

    #[test]
    fn emitted_errors_reach_the_error_listener_until_closed() {
        let dir = tempfile::tempdir().unwrap();
        let errors = Arc::new(Mutex::new(0));
        let e = errors.clone();
        let watcher =
            watch_with_error_handler(dir.path().to_str().unwrap(), Arc::new(|_, _| {}), Arc::new(move || *e.lock() += 1))
                .unwrap();
        watcher.emit_error();
        assert_eq!(*errors.lock(), 1);
        watcher.close();
        watcher.emit_error();
        assert_eq!(*errors.lock(), 1);
    }

    #[test]
    fn watch_file_polls_stats() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("HEAD");
        std::fs::write(&file, "ref: refs/heads/main\n").unwrap();
        let calls: Arc<Mutex<Vec<(WatchFileStats, WatchFileStats)>>> = Arc::default();
        let sink = calls.clone();
        let watcher = watch_file(file.to_str().unwrap(), 20, Arc::new(move |c, p| sink.lock().push((*c, *p))));
        std::fs::write(&file, "ref: refs/heads/feature\n").unwrap();
        assert!(wait_until(|| !calls.lock().is_empty()));
        let (current, previous) = calls.lock()[0];
        assert_eq!(previous.size, 21);
        assert_eq!(current.size, 24);
        std::fs::remove_file(&file).unwrap();
        assert!(wait_until(|| calls.lock().last().is_some_and(|(c, _)| *c == WatchFileStats::default())));
        watcher.close();
    }
}
