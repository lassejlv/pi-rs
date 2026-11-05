use pi_chord::sqlite::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

async fn database() -> NativeSqliteDatabase {
    open_native_sqlite_database(":memory:", Default::default())
        .await
        .unwrap()
}
fn integer(row: &SqliteRow, key: &str) -> i64 {
    match row[key] {
        SqliteValue::Integer(n) => n,
        SqliteValue::Real(n) => n as i64,
        _ => panic!("not numeric"),
    }
}
fn migration(version: u32, statements: &[&str]) -> SqliteMigration {
    SqliteMigration {
        version,
        statements: statements.iter().map(|s| (*s).into()).collect(),
    }
}

#[tokio::test]
async fn native_connection_defaults_explicit_settings_and_parent_directories() {
    for options in [
        NativeSqliteDatabaseOptions::default(),
        NativeSqliteDatabaseOptions {
            busy_timeout_ms: 123,
            wal_auto_checkpoint_pages: 0,
        },
    ] {
        let db = open_native_sqlite_database(":memory:", options)
            .await
            .unwrap();
        assert_eq!(
            integer(
                &db.get("PRAGMA busy_timeout", &[]).await.unwrap().unwrap(),
                "timeout"
            ),
            options.busy_timeout_ms as i64
        );
        assert_eq!(
            integer(
                &db.get("PRAGMA wal_autocheckpoint", &[])
                    .await
                    .unwrap()
                    .unwrap(),
                "wal_autocheckpoint"
            ),
            options.wal_auto_checkpoint_pages as i64
        );
        assert_eq!(
            integer(
                &db.get("PRAGMA synchronous", &[]).await.unwrap().unwrap(),
                "synchronous"
            ),
            1
        );
        db.close().await.unwrap();
        db.close().await.unwrap();
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("parent/nested/probe.sqlite");
    let db = open_native_sqlite_database(&path, Default::default())
        .await
        .unwrap();
    db.exec("CREATE TABLE probe (value TEXT)").await.unwrap();
    db.close().await.unwrap();
    assert!(path.is_file());
}

#[tokio::test]
async fn commit_rollback_and_stale_handles_preserve_callback_error_identity() {
    let db = database().await;
    db.exec("CREATE TABLE probe (value INTEGER)").await.unwrap();
    let handle = db
        .transaction(|tx| async move {
            tx.run("INSERT INTO probe VALUES (?)", &[1.into()]).await?;
            Ok(tx)
        })
        .await
        .unwrap();
    assert_eq!(
        integer(
            &db.get("SELECT value FROM probe", &[])
                .await
                .unwrap()
                .unwrap(),
            "value"
        ),
        1
    );
    for result in [
        handle.exec("INSERT INTO probe VALUES (2)").await,
        handle
            .run("INSERT INTO probe VALUES (?)", &[3.into()])
            .await,
    ] {
        assert_eq!(
            result.unwrap_err().to_string(),
            "SQLite transaction handle is no longer active"
        );
    }
    let failure = SqliteError::new("expected failure");
    let error = db
        .transaction::<(), _, _>({
            let failure = failure.clone();
            move |tx| async move {
                tx.exec("INSERT INTO probe VALUES (4)").await?;
                Err(failure)
            }
        })
        .await
        .unwrap_err();
    assert!(error.same_error(&failure));
    assert_eq!(
        db.all("SELECT value FROM probe", &[]).await.unwrap().len(),
        1
    );
    db.close().await.unwrap();
    assert_eq!(
        handle.get("SELECT 1", &[]).await.unwrap_err().to_string(),
        "SQLite transaction handle is no longer active"
    );
    assert!(db.exec("SELECT 1").await.is_err());
}

#[tokio::test]
async fn operations_and_transactions_queued_during_rollback_keep_call_order() {
    let db = database().await;
    db.exec("CREATE TABLE probe (value INTEGER)").await.unwrap();
    let (started, entered) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let first = db.transaction::<(), _, _>(move |tx| async move {
        tx.exec("INSERT INTO probe VALUES (1)").await?;
        started.send(()).unwrap();
        gate.await.unwrap();
        Err(SqliteError::new("roll back"))
    });
    entered.await.unwrap();
    let before = db.all("SELECT value FROM probe", &[]);
    let write = db.run("INSERT INTO probe VALUES (?)", &[2.into()]);
    let after = db.all("SELECT value FROM probe", &[]);
    let second_started = Arc::new(AtomicBool::new(false));
    let second = db.transaction({
        let started = second_started.clone();
        move |tx| async move {
            started.store(true, Ordering::SeqCst);
            tx.exec("INSERT INTO probe VALUES (3)").await
        }
    });
    let close = db.close();
    tokio::task::yield_now().await;
    assert!(!second_started.load(Ordering::SeqCst));
    release.send(()).unwrap();
    assert_eq!(first.await.unwrap_err().to_string(), "roll back");
    assert!(before.await.unwrap().is_empty());
    write.await.unwrap();
    let rows = after.await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(integer(&rows[0], "value"), 2);
    second.await.unwrap();
    close.await.unwrap();
    assert!(second_started.load(Ordering::SeqCst));
}

#[tokio::test]
async fn database_calls_from_inside_callback_wait_outside_its_rollback() {
    let db = database().await;
    db.exec("CREATE TABLE probe (value INTEGER)").await.unwrap();
    let (send, receive) = tokio::sync::oneshot::channel();
    let transaction = db.transaction::<(), _, _>({
        let db = db.clone();
        move |tx| async move {
            let outside = db.run("INSERT INTO probe VALUES (?)", &[2.into()]);
            send.send(outside).ok().unwrap();
            tx.exec("INSERT INTO probe VALUES (1)").await?;
            Err(SqliteError::new("roll back"))
        }
    });
    assert!(transaction.await.is_err());
    receive.await.unwrap().await.unwrap();
    let rows = db.all("SELECT value FROM probe", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(integer(&rows[0], "value"), 2);
    db.close().await.unwrap();
}

#[tokio::test]
async fn rollback_failure_is_distinct_from_the_original_callback_error() {
    let db = database().await;
    db.exec("CREATE TABLE probe (value INTEGER)").await.unwrap();
    let error = db
        .transaction::<(), _, _>(|tx| async move {
            tx.exec("INSERT INTO probe VALUES (1); COMMIT").await?;
            Err(SqliteError::new("after escaped commit"))
        })
        .await
        .unwrap_err();
    assert!(
        matches!(error.kind(),SqliteErrorKind::Rollback {primary,..} if primary.to_string()=="after escaped commit")
    );
    assert_eq!(
        error.to_string(),
        "SQLite transaction failed and rollback failed"
    );
    assert_eq!(
        db.all("SELECT value FROM probe", &[]).await.unwrap().len(),
        1
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn panicking_callback_rolls_back_and_queue_recovers() {
    let db = database().await;
    db.exec("CREATE TABLE probe (value INTEGER)").await.unwrap();
    let result = db
        .transaction::<(), _, _>(|tx| async move {
            tx.exec("INSERT INTO probe VALUES (1)").await?;
            panic!("callback panic")
        })
        .await;
    assert_eq!(result.unwrap_err().to_string(), "callback panic");
    assert!(
        db.all("SELECT value FROM probe", &[])
            .await
            .unwrap()
            .is_empty()
    );
    db.run("INSERT INTO probe VALUES (?)", &[2.into()])
        .await
        .unwrap();
    db.close().await.unwrap();
}

#[tokio::test]
async fn dropping_a_waiter_does_not_release_an_active_transaction() {
    let db = database().await;
    db.exec("CREATE TABLE probe (value INTEGER)").await.unwrap();
    let (started, entered) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let transaction = db.transaction(move |tx| async move {
        tx.exec("INSERT INTO probe VALUES (1)").await?;
        started.send(()).unwrap();
        gate.await.unwrap();
        Ok(())
    });
    entered.await.unwrap();
    drop(transaction);
    let after = tokio::spawn(db.all("SELECT value FROM probe", &[]));
    tokio::task::yield_now().await;
    assert!(!after.is_finished());
    release.send(()).unwrap();
    assert_eq!(after.await.unwrap().unwrap().len(), 1);
    db.close().await.unwrap();
}

#[tokio::test]
async fn migrations_are_contiguous_independent_atomic_and_reject_newer_schemas() {
    let db = database().await;
    for versions in [vec![0], vec![2], vec![1, 3], vec![1, 1]] {
        let migrations: Vec<_> = versions.into_iter().map(|v| migration(v, &[])).collect();
        assert_eq!(
            apply_sqlite_migrations(&db, &migrations, "app_schema")
                .await
                .unwrap_err()
                .to_string(),
            "SQLite migrations must have contiguous versions starting at 1"
        );
    }
    for prefix in [
        "",
        "1bad",
        "bad-prefix",
        "bad; DROP TABLE notes",
        "sqlite_app",
        "SQLITE_app",
    ] {
        assert!(validate_sqlite_schema_prefix(prefix).is_err());
        assert!(apply_sqlite_migrations(&db, &[], prefix).await.is_err());
    }
    assert!(
        db.all("SELECT name FROM sqlite_schema", &[])
            .await
            .unwrap()
            .is_empty()
    );
    for prefix in ["pi_", "_private", "app2"] {
        validate_sqlite_schema_prefix(prefix).unwrap();
    }
    let first = migration(
        1,
        &[
            "CREATE TABLE notes (body TEXT)",
            "INSERT INTO notes VALUES ('kept')",
        ],
    );
    apply_sqlite_migrations(&db, std::slice::from_ref(&first), "notes_schema")
        .await
        .unwrap();
    apply_sqlite_migrations(&db, std::slice::from_ref(&first), "notes_schema")
        .await
        .unwrap();
    apply_sqlite_migrations(&db, &[], "other_schema")
        .await
        .unwrap();
    apply_sqlite_migrations(
        &db,
        &[
            first.clone(),
            migration(2, &["ALTER TABLE notes ADD COLUMN title TEXT"]),
        ],
        "notes_schema",
    )
    .await
    .unwrap();
    assert_eq!(
        integer(
            &db.get("SELECT version FROM notes_schema", &[])
                .await
                .unwrap()
                .unwrap(),
            "version"
        ),
        2
    );
    assert_eq!(
        integer(
            &db.get("SELECT version FROM other_schema", &[])
                .await
                .unwrap()
                .unwrap(),
            "version"
        ),
        0
    );
    assert_eq!(
        db.get("SELECT title FROM notes", &[])
            .await
            .unwrap()
            .unwrap()["title"],
        SqliteValue::Null
    );
    assert_eq!(
        apply_sqlite_migrations(&db, &[first], "notes_schema")
            .await
            .unwrap_err()
            .to_string(),
        "SQLite schema version 2 is newer than supported version 1"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn a_failed_later_migration_rolls_back_bootstrap_and_all_pending_versions() {
    let db = database().await;
    let first = migration(
        1,
        &[
            "CREATE TABLE migration_first (value TEXT) STRICT",
            "INSERT INTO migration_first VALUES ('retained')",
        ],
    );
    let bad = migration(
        2,
        &[
            "CREATE TABLE migration_second (value TEXT) STRICT",
            "THIS IS NOT SQL",
        ],
    );
    assert!(
        apply_sqlite_migrations(&db, &[first.clone(), bad], "app_schema")
            .await
            .is_err()
    );
    assert!(
        db.all("SELECT name FROM sqlite_schema", &[])
            .await
            .unwrap()
            .is_empty()
    );
    apply_sqlite_migrations(
        &db,
        &[
            first,
            migration(2, &["CREATE TABLE migration_second (value TEXT) STRICT"]),
        ],
        "app_schema",
    )
    .await
    .unwrap();
    assert_eq!(
        integer(
            &db.get("SELECT version FROM app_schema", &[])
                .await
                .unwrap()
                .unwrap(),
            "version"
        ),
        2
    );
    assert_eq!(
        db.get("SELECT value FROM migration_first", &[])
            .await
            .unwrap()
            .unwrap()["value"],
        SqliteValue::Text("retained".into())
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn blobs_null_reals_and_missing_rows_survive_bindings_and_results() {
    let db = database().await;
    db.exec("CREATE TABLE probe (id INTEGER, data BLOB, n REAL, text TEXT)")
        .await
        .unwrap();
    db.run(
        "INSERT INTO probe VALUES (?,?,?,?)",
        &[
            1.into(),
            SqliteValue::Blob(vec![0, 1, 255]),
            SqliteValue::Real(1.5),
            SqliteValue::Null,
        ],
    )
    .await
    .unwrap();
    let row = db
        .get("SELECT * FROM probe WHERE id=?", &[1.into()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row["data"], SqliteValue::Blob(vec![0, 1, 255]));
    assert_eq!(row["n"], SqliteValue::Real(1.5));
    assert_eq!(row["text"], SqliteValue::Null);
    assert!(
        db.get("SELECT * FROM probe WHERE id=?", &[2.into()])
            .await
            .unwrap()
            .is_none()
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn a_submitted_transaction_operation_finishes_inside_its_transaction_even_without_a_waiter() {
    let db = database().await;
    db.exec("CREATE TABLE probe (value INTEGER)").await.unwrap();
    db.transaction(|tx| async move {
        drop(tx.run("INSERT INTO probe VALUES (?)", &[1.into()]));
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        db.all("SELECT value FROM probe", &[]).await.unwrap().len(),
        1
    );
    db.close().await.unwrap();
}
