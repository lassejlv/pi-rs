//! A deep chain of owned tasks that all end at once, as in the stock "forgets a 20,000-deep chain of ended tasks"
//! retention case, measured natively on SQLite so the time is the scheduler's and the storage's, not a transport's.
//!
//!   PI_DURABLE_CHAIN_DEPTH=20000 cargo test --release -p pi-durable --test scheduler_chain_retention -- --ignored --nocapture
use indexmap::IndexMap;
use pi_durable::{
    harness::{catalog::*, scheduler::*},
    storage::sqlite::open_native_sqlite_storage,
    *,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

type JsonTask = Task<JsonValue, JsonValue, JsonValue>;

/// When the first and the last phase started, to separate reservation from the commits that end the chain.
static PHASES: parking_lot::Mutex<Option<(Instant, Instant)>> = parking_lot::Mutex::new(None);

fn hold() -> JsonTask {
    let phase: PhaseHandler<JsonValue, JsonValue, JsonValue> = Arc::new(|_, runtime, context| {
        let now = Instant::now();
        let mut phases = PHASES.lock();
        *phases = Some(phases.map_or((now, now), |(first, _)| (first, now)));
        drop(phases);
        Box::pin(async move {
            runtime
                .commit(
                    |_, _| async {
                        Ok(Some(NextTaskState::Terminal {
                            outcome: TaskOutcome::Completed {
                                result: JsonValue::Null,
                            },
                        }))
                    },
                    &context,
                )
                .await
        })
    });
    define_task(TaskDefinition {
        name: "test.retention.hold".into(),
        version: 1,
        initial: Arc::new(|_| Ok(pi_chord::json::parse(r#"{"phase":"hold"}"#).unwrap())),
        phases: IndexMap::from([("hold".into(), phase)]),
        abort: Arc::new(|_, runtime, context| {
            Box::pin(async move {
                runtime
                    .commit(
                        |_, _| async {
                            Ok(Some(NextTaskState::Terminal {
                                outcome: TaskOutcome::Aborted {
                                    reason: None,
                                    result: None,
                                },
                            }))
                        },
                        &context,
                    )
                    .await
            })
        }),
        migrate: None,
        hooks: None,
    })
}

struct Timings {
    create: Duration,
    /// From the creating commit to the first and the last phase start.
    first_phase: Duration,
    last_phase: Duration,
    run: Duration,
    total: Duration,
}

async fn chain(depth: usize, storage: Arc<dyn Storage>) -> Timings {
    let task = hold();
    let catalog = TaskCatalog::default();
    catalog
        .replace([InstalledTask {
            task: task.erased(),
            extension: Some("tests".into()),
        }])
        .unwrap();
    let context = Context::default();
    let scheduler = TaskScheduler::open(
        storage,
        catalog,
        SchedulerOptions::default(),
        SessionOptions::default(),
        &context,
    )
    .await
    .unwrap();
    let root = scheduler
        .session()
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &context,
        )
        .await
        .unwrap();
    let baseline = scheduler.index_sizes();
    let started = Instant::now();
    let created = task.clone();
    let (first, leaf) = scheduler
        .session()
        .commit(
            move |tx| async move {
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(root);
                let first = tx
                    .create_task(&created, JsonValue::Null, options)
                    .await?
                    .untyped();
                let mut owner = first;
                for _ in 0..depth {
                    owner = tx
                        .create_task(
                            &created,
                            JsonValue::Null,
                            TaskOptions::new(TaskOwnership::Task(owner)),
                        )
                        .await?
                        .untyped();
                }
                Ok((first, owner))
            },
            &context,
        )
        .await
        .unwrap();
    let create = started.elapsed();
    *PHASES.lock() = None;
    scheduler.resume().unwrap();
    scheduler.wait_for_task(leaf, &context).await.unwrap();
    scheduler.wait_for_task(first, &context).await.unwrap();
    scheduler.wait_for_idle(None, &context).await.unwrap();
    let total = started.elapsed();
    assert_eq!(scheduler.index_sizes(), baseline);
    scheduler.session().close(&context).await.unwrap();
    let (first, last) = PHASES.lock().unwrap();
    Timings {
        create,
        first_phase: first - started - create,
        last_phase: last - started - create,
        run: total - create,
        total,
    }
}

async fn sqlite_chain(depth: usize) -> Timings {
    let directory = tempfile::tempdir().unwrap();
    let storage =
        open_native_sqlite_storage(directory.path().join("session.sqlite"), Default::default())
            .await
            .unwrap();
    chain(depth, Arc::new(storage)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark; run with --ignored --nocapture"]
async fn deep_chain_benchmark() {
    let depth = std::env::var("PI_DURABLE_CHAIN_DEPTH")
        .ok()
        .and_then(|depth| depth.parse().ok())
        .unwrap_or(20_000);
    for (name, timings) in [
        ("memory", chain(depth, Arc::new(MemoryStorage::new())).await),
        ("sqlite", sqlite_chain(depth).await),
    ] {
        println!(
            "{name} depth={depth} create={:?} first_phase={:?} last_phase={:?} run={:?} total={:?}",
            timings.create, timings.first_phase, timings.last_phase, timings.run, timings.total
        );
    }
}

/// What one single-task SQLite commit costs through the native database's worker thread, against the same
/// statements run in place. Each statement of a native transaction is one round trip to the worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark; run with --ignored --nocapture"]
async fn sqlite_commit_round_trips() {
    use pi_chord::sqlite::{SqliteDatabaseExt, SqliteExecutor, open_native_sqlite_database};
    let commits = 5_000i64;
    let directory = tempfile::tempdir().unwrap();
    let database =
        open_native_sqlite_database(directory.path().join("worker.sqlite"), Default::default())
            .await
            .unwrap();
    database
        .exec("CREATE TABLE ids (id INTEGER PRIMARY KEY, kind TEXT); CREATE TABLE tasks (id INTEGER PRIMARY KEY, record TEXT); CREATE TABLE meta (singleton INTEGER PRIMARY KEY, next INTEGER); INSERT INTO meta VALUES (1, 0)")
        .await
        .unwrap();
    let started = Instant::now();
    for id in 0..commits {
        database
            .transaction(move |tx| async move {
                tx.get("SELECT next FROM meta WHERE singleton = 1", &[]).await?;
                tx.get("SELECT kind FROM ids WHERE id = ?", &[id.into()]).await?;
                tx.run("INSERT OR IGNORE INTO ids (id, kind) VALUES (?, 'task')", &[id.into()]).await?;
                tx.run("INSERT INTO tasks (id, record) VALUES (?, '{}') ON CONFLICT(id) DO UPDATE SET record = excluded.record", &[id.into()]).await?;
                tx.run("UPDATE meta SET next = ? WHERE singleton = 1", &[id.into()]).await?;
                Ok(())
            })
            .await
            .unwrap();
    }
    let worker = started.elapsed();
    let connection = rusqlite::Connection::open(directory.path().join("inline.sqlite")).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .unwrap();
    connection
        .execute_batch("CREATE TABLE ids (id INTEGER PRIMARY KEY, kind TEXT); CREATE TABLE tasks (id INTEGER PRIMARY KEY, record TEXT); CREATE TABLE meta (singleton INTEGER PRIMARY KEY, next INTEGER); INSERT INTO meta VALUES (1, 0)")
        .unwrap();
    let started = Instant::now();
    for id in 0..commits {
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        connection
            .prepare_cached("SELECT next FROM meta WHERE singleton = 1")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap();
        connection
            .prepare_cached("SELECT kind FROM ids WHERE id = ?")
            .unwrap()
            .query([id])
            .unwrap()
            .next()
            .unwrap();
        connection
            .prepare_cached("INSERT OR IGNORE INTO ids (id, kind) VALUES (?, 'task')")
            .unwrap()
            .execute([id])
            .unwrap();
        connection.prepare_cached("INSERT INTO tasks (id, record) VALUES (?, '{}') ON CONFLICT(id) DO UPDATE SET record = excluded.record").unwrap().execute([id]).unwrap();
        connection
            .prepare_cached("UPDATE meta SET next = ? WHERE singleton = 1")
            .unwrap()
            .execute([id])
            .unwrap();
        connection.execute_batch("COMMIT").unwrap();
    }
    let inline = started.elapsed();
    println!(
        "commits={commits} worker={worker:?} ({:.1}us each) inline={inline:?} ({:.1}us each)",
        worker.as_secs_f64() * 1e6 / commits as f64,
        inline.as_secs_f64() * 1e6 / commits as f64
    );
}

/// A transaction that cannot take the write lock at once waits for it on the worker, as before
/// uncontended transactions began in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contended_sqlite_transactions_wait_for_the_write_lock() {
    use pi_chord::sqlite::{
        SqliteDatabase, SqliteDatabaseExt, SqliteExecutor, open_native_sqlite_database,
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("shared.sqlite");
    let first = open_native_sqlite_database(&path, Default::default())
        .await
        .unwrap();
    first
        .exec("CREATE TABLE rows (id INTEGER PRIMARY KEY)")
        .await
        .unwrap();
    let second = open_native_sqlite_database(&path, Default::default())
        .await
        .unwrap();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let (holding, held) = tokio::sync::oneshot::channel::<()>();
    let holder = first.transaction(move |tx| async move {
        tx.run("INSERT INTO rows (id) VALUES (1)", &[]).await?;
        let _ = holding.send(());
        let _ = released.await;
        Ok(())
    });
    held.await.unwrap();
    let waiter = second.transaction(|tx| async move {
        tx.run("INSERT INTO rows (id) VALUES (2)", &[]).await?;
        Ok(())
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    release.send(()).unwrap();
    holder.await.unwrap();
    waiter.await.unwrap();
    let rows = second
        .all("SELECT id FROM rows ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    first.close().await.unwrap();
    second.close().await.unwrap();
}
