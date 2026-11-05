use super::*;
use futures::FutureExt;
use parking_lot::Mutex;
use std::sync::atomic::AtomicUsize;

struct Handle(Arc<AtomicUsize>);
impl Drop for Handle {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
fn counting_opener(live: Arc<AtomicUsize>, opens: Arc<AtomicUsize>) -> NativeWatchOpener {
    Arc::new(move |_, _, _| {
        live.fetch_add(1, Ordering::AcqRel);
        opens.fetch_add(1, Ordering::AcqRel);
        Ok(Box::new(Handle(live.clone())))
    })
}
fn native_options(opener: NativeWatchOpener) -> NativeWatchOptions {
    NativeWatchOptions {
        mode: Some(WatchMode::Native),
        opener: Some(opener),
        ..Default::default()
    }
}

#[tokio::test]
async fn polling_hashes_detect_content_changes_with_identical_metadata_and_callbacks_survive_panics()
 {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("file");
    std::fs::write(&path, b"aa").unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let frames = Arc::new(Mutex::new(Vec::new()));
    let seen = frames.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    let watcher = NativeFileWatcher::open(
        vec![WatchTarget::new(path.clone())],
        Arc::new(move |change| {
            seen.lock().push(change);
            if called.fetch_add(1, Ordering::AcqRel) == 0 {
                panic!("intentional watch callback failure");
            }
        }),
        NativeWatchOptions {
            mode: Some(WatchMode::Polling),
            poll_interval_ms: Some(10.0),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for text in [b"bb", b"cc"] {
        let before = frames.lock().len();
        std::fs::write(&path, text).unwrap();
        std::fs::File::open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while frames.lock().len() == before {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
    watcher.close(&Context::default()).await.unwrap();
    let count = frames.lock().len();
    std::fs::write(&path, b"dd").unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(frames.lock().len(), count);
    assert!(frames.lock().iter().all(
        |frame| matches!(frame,WatchChange::Paths(paths) if paths==std::slice::from_ref(&path))
    ));
}

#[tokio::test]
async fn exhausted_native_coverage_closes_handles_and_announces_polling() {
    let directory = tempfile::tempdir().unwrap();
    let live = Arc::new(AtomicUsize::new(0));
    let opened = Arc::new(AtomicUsize::new(0));
    let handles = counting_opener(live.clone(), opened.clone());
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempted = attempts.clone();
    let opener: NativeWatchOpener = Arc::new(move |path, recursive, callback| {
        if attempted.fetch_add(1, Ordering::AcqRel) == 1 {
            return Err(FileError::new(
                FileErrorCode::NotSupported,
                "watch limit",
                None,
            ));
        }
        handles(path, recursive, callback)
    });
    let frames = Arc::new(Mutex::new(Vec::new()));
    let seen = frames.clone();
    let watcher = NativeFileWatcher::open(
        vec![WatchTarget::new(directory.path())],
        Arc::new(move |change| seen.lock().push(change)),
        NativeWatchOptions {
            poll_interval_ms: Some(10.0),
            ..native_options(opener)
        },
    )
    .await
    .unwrap();
    assert_eq!(watcher.mode(), WatchMode::Polling);
    assert_eq!(opened.load(Ordering::Acquire), 1);
    assert_eq!(live.load(Ordering::Acquire), 0);
    assert!(matches!(frames.lock().as_slice(), [WatchChange::Overflow]));
    std::fs::write(directory.path().join("new"), b"x").unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while frames.lock().len() < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    watcher.close(&Context::default()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_during_a_scan_does_not_reinstall_coverage() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("tree");
    std::fs::create_dir(&root).unwrap();
    let live = Arc::new(AtomicUsize::new(0));
    let opened = Arc::new(AtomicUsize::new(0));
    let scans = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (entered, reached) = oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered)));
    let releasing = release.clone();
    let source: ScanSource = Arc::new(move |targets, mode, max| {
        let blocking = scans.fetch_add(1, Ordering::AcqRel) == 2;
        let entered = entered.clone();
        let release = releasing.clone();
        Box::pin(async move {
            if blocking {
                let _ = entered.lock().take().unwrap().send(());
                release.acquire().await.unwrap().forget();
            }
            snapshot::capture(&targets, mode, max)
        })
    });
    let watcher = NativeFileWatcher::start(
        vec![WatchTarget::new(&root)],
        Arc::new(|_| {}),
        native_options(counting_opener(live.clone(), opened.clone())),
        source,
    )
    .await
    .unwrap();
    let before = opened.load(Ordering::Acquire);
    std::fs::rename(&root, directory.path().join("old")).unwrap();
    std::fs::create_dir(&root).unwrap();
    watcher
        .inner
        .send
        .send(Command::Native {
            root: root.clone(),
            serial: 0,
            linked: false,
            event: NativeWatchEvent::Change(None),
            at: Instant::now(),
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), reached)
        .await
        .unwrap()
        .unwrap();
    let closing = watcher.close(&Context::default());
    release.add_permits(1);
    closing.await.unwrap();
    assert_eq!(
        opened.load(Ordering::Acquire),
        before,
        "closed during scan, then installed another native handle"
    );
    assert_eq!(live.load(Ordering::Acquire), 0);
}

#[test]
fn runtime_shutdown_releases_native_handles_even_while_the_public_watcher_is_retained() {
    let directory = tempfile::tempdir().unwrap();
    let live = Arc::new(AtomicUsize::new(0));
    let opened = Arc::new(AtomicUsize::new(0));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let watcher = runtime.block_on(async {
        NativeFileWatcher::open(
            vec![WatchTarget::new(directory.path())],
            Arc::new(|_| {}),
            native_options(counting_opener(live.clone(), opened)),
        )
        .await
    });
    let watcher = watcher.unwrap();
    assert!(live.load(Ordering::Acquire) > 0);
    drop(runtime);
    assert_eq!(live.load(Ordering::Acquire), 0);
    futures::executor::block_on(watcher.close(&Context::default())).unwrap();
}

struct BlockingHandle {
    live: Arc<AtomicUsize>,
    entered: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    release: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
}
impl Drop for BlockingHandle {
    fn drop(&mut self) {
        if let Some(entered) = self.entered.lock().take() {
            let _ = entered.send(());
            self.release
                .lock()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
        }
        self.live.fetch_sub(1, Ordering::AcqRel);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_initialization_releases_coverage_before_returning_the_error() {
    let directory = tempfile::tempdir().unwrap();
    let live = Arc::new(AtomicUsize::new(0));
    let owning = live.clone();
    let (entered, reached) = oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered)));
    let (release, released) = std::sync::mpsc::channel();
    let released = Arc::new(Mutex::new(released));
    let opener: NativeWatchOpener = Arc::new(move |_, _, _| {
        owning.fetch_add(1, Ordering::AcqRel);
        Ok(Box::new(BlockingHandle {
            live: owning.clone(),
            entered: entered.clone(),
            release: released.clone(),
        }))
    });
    let scans = Arc::new(AtomicUsize::new(0));
    let source: ScanSource = Arc::new(move |targets, mode, max| {
        let fails = scans.fetch_add(1, Ordering::AcqRel) > 0;
        Box::pin(async move {
            if fails {
                Err(FileError::new(
                    FileErrorCode::Invalid,
                    "second scan failed",
                    None,
                ))
            } else {
                snapshot::capture(&targets, mode, max)
            }
        })
    });
    let mut opening = NativeFileWatcher::start(
        vec![WatchTarget::new(directory.path())],
        Arc::new(|_| {}),
        native_options(opener),
        source,
    );
    tokio::time::timeout(Duration::from_secs(2), reached)
        .await
        .unwrap()
        .unwrap();
    let returned_early = opening.as_mut().now_or_never();
    release.send(()).unwrap();
    assert!(
        returned_early.is_none(),
        "initialization failed before its native handles finished closing"
    );
    let error = opening.await.err().unwrap();
    assert_eq!(error.to_string(), "second scan failed");
    assert_eq!(live.load(Ordering::Acquire), 0);
}
