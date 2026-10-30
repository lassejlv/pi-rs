//! Snapshot reconciliation driven by native events or explicit polling.
mod backend;
mod snapshot;
use super::filesystem::*;
use crate::Context;
use snapshot::{Kind, Scan, Snapshot, ancestors, excluded};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::Instant,
};

#[derive(Clone, Debug)]
pub enum NativeWatchEvent {
    Change(Option<PathBuf>),
    Error,
    Overflow,
}
pub type NativeWatchCallback = Arc<dyn Fn(NativeWatchEvent) + Send + Sync>;
pub trait NativeWatchHandle: Send {}
impl<T: Send> NativeWatchHandle for T {}
pub type NativeWatchOpener = Arc<
    dyn Fn(PathBuf, bool, NativeWatchCallback) -> FileResult<Box<dyn NativeWatchHandle>>
        + Send
        + Sync,
>;
#[derive(Clone, Default)]
pub struct NativeWatchOptions {
    pub mode: Option<WatchMode>,
    pub poll_interval_ms: Option<f64>,
    pub max_directories: Option<f64>,
    pub opener: Option<NativeWatchOpener>,
}
type ScanSource = Arc<dyn Fn(Vec<WatchTarget>, WatchMode, f64) -> FileFuture<Scan> + Send + Sync>;
struct Inner {
    closed: AtomicBool,
    mode: AtomicU8,
    send: mpsc::UnboundedSender<Command>,
    done: watch::Sender<bool>,
}
pub struct NativeFileWatcher {
    inner: Arc<Inner>,
}
enum Command {
    Native {
        root: PathBuf,
        serial: u64,
        linked: bool,
        event: NativeWatchEvent,
        at: Instant,
    },
    Close,
}
struct Installed {
    _handle: Box<dyn NativeWatchHandle>,
    dev: u64,
    ino: u64,
    serial: u64,
}
struct Driver {
    inner: Arc<Inner>,
    targets: Vec<WatchTarget>,
    callback: WatchCallback,
    options: NativeWatchOptions,
    source: ScanSource,
    snapshot: Snapshot,
    installed: BTreeMap<PathBuf, Installed>,
    events: BTreeSet<PathBuf>,
    timer: Option<Instant>,
    settle_timer: Option<Instant>,
    serial: u64,
}
fn mode(value: u8) -> WatchMode {
    if value == 0 {
        WatchMode::Native
    } else {
        WatchMode::Polling
    }
}
fn timer_duration(ms: f64) -> Duration {
    let ms = if !ms.is_finite() || !(1.0..=2_147_483_647.0).contains(&ms) {
        1.0
    } else {
        ms.floor()
    };
    Duration::from_millis(ms as u64)
}
impl Inner {
    fn stop(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            let _ = self.send.send(Command::Close);
        }
    }
}
impl NativeFileWatcher {
    /// Targets supplied here are absolute. NativeFileSystem resolves its targets
    /// at watch admission before invoking this constructor.
    pub fn open(
        targets: Vec<WatchTarget>,
        callback: WatchCallback,
        options: NativeWatchOptions,
    ) -> FileFuture<Arc<Self>> {
        Self::start(
            targets,
            callback,
            options,
            Arc::new(|targets, mode, max| {
                file_job(async move {
                    tokio::task::spawn_blocking(move || snapshot::capture(&targets, mode, max))
                        .await
                        .map_err(|error| {
                            FileError::new(FileErrorCode::Unknown, error.to_string(), None)
                        })?
                })
            }),
        )
    }
    fn start(
        targets: Vec<WatchTarget>,
        callback: WatchCallback,
        options: NativeWatchOptions,
        source: ScanSource,
    ) -> FileFuture<Arc<Self>> {
        let (send, mut receive) = mpsc::unbounded_channel();
        let (done, _) = watch::channel(false);
        let inner = Arc::new(Inner {
            closed: AtomicBool::new(false),
            mode: AtomicU8::new(0),
            send,
            done,
        });
        let watcher = Arc::new(Self {
            inner: inner.clone(),
        });
        let (ready, opened) = oneshot::channel();
        tokio::spawn(async move {
            let initial = match options.mode {
                Some(mode) => mode,
                None if cfg!(windows) => WatchMode::Polling,
                None => {
                    let paths = targets.clone();
                    if tokio::task::spawn_blocking(move || snapshot::unreliable(&paths))
                        .await
                        .unwrap_or(false)
                    {
                        WatchMode::Polling
                    } else {
                        WatchMode::Native
                    }
                }
            };
            inner
                .mode
                .store(u8::from(initial == WatchMode::Polling), Ordering::Release);
            let mut driver = Driver {
                inner,
                targets,
                callback,
                options,
                source,
                snapshot: Snapshot::new(),
                installed: BTreeMap::new(),
                events: BTreeSet::new(),
                timer: None,
                settle_timer: None,
                serial: 0,
            };
            if let Err(error) = driver.sync(false).await {
                drop(driver);
                let _ = ready.send(Err(error));
                return;
            }
            driver.schedule_poll();
            if ready.send(Ok(())).is_err() {
                return;
            }
            while !driver.inner.closed.load(Ordering::Acquire) {
                enum Wake {
                    Command(Option<Command>),
                    Timer,
                    Settle,
                }
                let wake = tokio::select! {
                    command=receive.recv()=>Wake::Command(command),
                    _=async {match driver.timer{Some(at)=>tokio::time::sleep_until(at).await,None=>std::future::pending().await}}=>Wake::Timer,
                    _=async {match driver.settle_timer{Some(at)=>tokio::time::sleep_until(at).await,None=>std::future::pending().await}}=>Wake::Settle,
                };
                match wake {
                    Wake::Command(Some(Command::Native {
                        root,
                        serial,
                        linked,
                        event,
                        at,
                    })) => driver.native(root, serial, linked, event, at),
                    Wake::Command(Some(Command::Close)) | Wake::Command(None) => break,
                    Wake::Timer => {
                        driver.timer = None;
                        driver.flush().await;
                        driver.schedule_poll();
                    }
                    Wake::Settle => {
                        driver.settle_timer = None;
                        driver.flush().await;
                    }
                }
            }
        });
        Box::pin(async move {
            opened.await.map_err(|_| {
                FileError::new(
                    FileErrorCode::Unknown,
                    "Watcher initialization stopped",
                    None,
                )
            })??;
            Ok(watcher)
        })
    }
}

#[cfg(test)]
mod tests;
impl FileWatcher for NativeFileWatcher {
    fn mode(&self) -> WatchMode {
        mode(self.inner.mode.load(Ordering::Acquire))
    }
    fn close(&self, _context: &Context) -> FileFuture<()> {
        self.inner.stop();
        let mut done = self.inner.done.subscribe();
        Box::pin(async move {
            while !*done.borrow_and_update() {
                if done.changed().await.is_err() {
                    break;
                }
            }
            Ok(())
        })
    }
}
impl Drop for NativeFileWatcher {
    fn drop(&mut self) {
        self.inner.stop();
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        self.inner.closed.store(true, Ordering::Release);
        self.installed.clear();
        self.inner.done.send_replace(true);
    }
}
impl Driver {
    fn mode(&self) -> WatchMode {
        mode(self.inner.mode.load(Ordering::Acquire))
    }
    fn deliver(&self, change: WatchChange) {
        if self.inner.closed.load(Ordering::Acquire) {
            return;
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.callback)(change)));
    }
    fn schedule_poll(&mut self) {
        if !self.inner.closed.load(Ordering::Acquire)
            && self.mode() == WatchMode::Polling
            && self.timer.is_none()
        {
            self.timer = Some(
                Instant::now() + timer_duration(self.options.poll_interval_ms.unwrap_or(2000.0)),
            );
        }
    }
    fn schedule_flush(&mut self, at: Instant) {
        if !self.inner.closed.load(Ordering::Acquire) && self.timer.is_none() {
            self.timer = Some(at + Duration::from_millis(50));
        }
    }
    fn native(
        &mut self,
        root: PathBuf,
        serial: u64,
        linked: bool,
        event: NativeWatchEvent,
        at: Instant,
    ) {
        if self.inner.closed.load(Ordering::Acquire) {
            return;
        }
        match event {
            NativeWatchEvent::Error => {
                if self
                    .installed
                    .get(&root)
                    .is_some_and(|entry| entry.serial == serial)
                {
                    self.installed.remove(&root);
                }
                self.schedule_flush(at);
            }
            NativeWatchEvent::Overflow => self.polling(),
            NativeWatchEvent::Change(name) if linked => {
                self.events.insert(root);
                self.schedule_flush(at);
                let _ = name;
            }
            NativeWatchEvent::Change(name) => {
                let path = name
                    .as_ref()
                    .map_or_else(|| root.clone(), |name| root.join(name));
                let relevant = self.in_scope(&path);
                if relevant {
                    self.events.insert(self.reported(&path));
                }
                if relevant || name.is_none() {
                    self.schedule_flush(at);
                }
            }
        }
    }
    fn polling(&mut self) {
        if self.mode() == WatchMode::Polling {
            return;
        }
        self.inner.mode.store(1, Ordering::Release);
        self.installed.clear();
        self.deliver(WatchChange::Overflow);
        self.timer = None;
        self.schedule_poll();
    }
    async fn flush(&mut self) {
        let events = std::mem::take(&mut self.events);
        match self.sync(true).await {
            Ok(mut changed) => {
                changed.extend(events);
                if !changed.is_empty() {
                    let mut changed = changed.into_iter().collect::<Vec<_>>();
                    changed.sort_by(|a, b| {
                        a.to_string_lossy()
                            .encode_utf16()
                            .cmp(b.to_string_lossy().encode_utf16())
                    });
                    self.deliver(WatchChange::Paths(changed));
                }
            }
            Err(error) => {
                self.deliver(WatchChange::Error(FileError::new(
                    if error.code() == FileErrorCode::PermissionDenied {
                        FileErrorCode::PermissionDenied
                    } else {
                        FileErrorCode::Invalid
                    },
                    error.to_string(),
                    None,
                )));
                self.inner.stop();
            }
        }
    }
    async fn sync(&mut self, mut report: bool) -> FileResult<BTreeSet<PathBuf>> {
        let mut changed = BTreeSet::new();
        for _ in 0..10 {
            if self.inner.closed.load(Ordering::Acquire) {
                break;
            }
            let scan = (self.source)(
                self.targets.clone(),
                self.mode(),
                self.options.max_directories.unwrap_or(10_000.0),
            )
            .await?;
            if self.inner.closed.load(Ordering::Acquire) {
                break;
            }
            if report {
                for (path, entry) in &scan.snapshot {
                    if self.snapshot.get(path) != Some(entry) {
                        changed.insert(self.reported(path));
                    }
                }
                for path in self.snapshot.keys() {
                    if !scan.snapshot.contains_key(path) {
                        changed.insert(self.reported(path));
                    }
                }
            }
            self.snapshot = scan.snapshot;
            if self.mode() == WatchMode::Polling || !self.reconcile(scan.linked_files) {
                break;
            }
            if cfg!(target_os = "macos") {
                self.settle_timer = Some(Instant::now() + Duration::from_millis(500));
            }
            report = true;
        }
        Ok(changed)
    }
    fn reported(&self, path: &std::path::Path) -> PathBuf {
        if self
            .targets
            .iter()
            .any(|target| path.starts_with(&target.path))
        {
            return path.into();
        }
        self.targets
            .iter()
            .find(|target| target.path.starts_with(path))
            .map_or_else(|| path.into(), |target| target.path.clone())
    }
    fn in_scope(&self, path: &std::path::Path) -> bool {
        for target in &self.targets {
            if target.path.starts_with(path) {
                return true;
            }
            let Ok(relative) = path.strip_prefix(&target.path) else {
                continue;
            };
            if relative.as_os_str().is_empty() {
                continue;
            }
            let parts = relative.components().collect::<Vec<_>>();
            if !target.recursive && parts.len() > 1 {
                continue;
            }
            if parts.iter().any(|part| excluded(target, part.as_os_str())) {
                continue;
            }
            return true;
        }
        false
    }
    fn reconcile(&mut self, linked_files: std::collections::HashSet<PathBuf>) -> bool {
        let mut wanted = BTreeMap::<PathBuf, bool>::new();
        for path in &linked_files {
            wanted.insert(path.clone(), false);
        }
        let per_directory = cfg!(any(target_os = "linux", target_os = "android"));
        for target in &self.targets {
            for ancestor in ancestors(&target.path) {
                if self
                    .snapshot
                    .get(&ancestor)
                    .is_some_and(|entry| entry.kind == Kind::Directory)
                {
                    wanted.entry(ancestor).or_insert(false);
                }
            }
            if self
                .snapshot
                .get(&target.path)
                .is_none_or(|entry| entry.kind != Kind::Directory)
            {
                continue;
            }
            wanted.insert(target.path.clone(), target.recursive && !per_directory);
            if target.recursive && per_directory {
                for (path, entry) in &self.snapshot {
                    if entry.kind == Kind::Directory
                        && path != &target.path
                        && path.starts_with(&target.path)
                    {
                        wanted.insert(path.clone(), false);
                    }
                }
            }
        }
        self.installed.retain(|path, installed| {
            wanted.contains_key(path)
                && self
                    .snapshot
                    .get(path)
                    .is_some_and(|entry| entry.dev == installed.dev && entry.ino == installed.ino)
        });
        let mut added = false;
        for (path, recursive) in wanted {
            if self.installed.contains_key(&path) {
                continue;
            }
            let Some(entry) = self.snapshot.get(&path) else {
                continue;
            };
            self.serial += 1;
            let serial = self.serial;
            let (send, inner, root, linked) = (
                self.inner.send.clone(),
                Arc::downgrade(&self.inner),
                path.clone(),
                linked_files.contains(&path),
            );
            let callback: NativeWatchCallback = Arc::new(move |event| {
                if inner
                    .upgrade()
                    .is_some_and(|inner| !inner.closed.load(Ordering::Acquire))
                {
                    let _ = send.send(Command::Native {
                        root: root.clone(),
                        serial,
                        linked,
                        event,
                        at: Instant::now(),
                    });
                }
            });
            let open = self
                .options
                .opener
                .as_ref()
                .cloned()
                .unwrap_or_else(|| Arc::new(backend::open));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                open(path.clone(), recursive, callback)
            }))
            .unwrap_or_else(|panic| {
                Err(FileError::new(
                    FileErrorCode::Unknown,
                    crate::session::panic_error(panic).to_string(),
                    None,
                ))
            });
            match result {
                Ok(handle) => {
                    self.installed.insert(
                        path,
                        Installed {
                            _handle: handle,
                            dev: entry.dev,
                            ino: entry.ino,
                            serial,
                        },
                    );
                    added = true;
                }
                Err(error)
                    if matches!(
                        error.code(),
                        FileErrorCode::NotFound | FileErrorCode::PermissionDenied
                    ) => {}
                Err(_) => {
                    self.polling();
                    return false;
                }
            }
        }
        added
    }
}
