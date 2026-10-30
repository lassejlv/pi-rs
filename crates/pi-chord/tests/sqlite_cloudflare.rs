use pi_chord::sqlite::{cloudflare::*, *};
use std::sync::Arc;
#[path = "support/sqlite_host.rs"]
mod sqlite_host;
use sqlite_host::Host;

fn database() -> (
    Arc<Host>,
    Arc<DurableObjectSqliteStorage>,
    DurableObjectSqliteDatabase,
) {
    let host = Arc::new(Host::new());
    let storage = Arc::new(DurableObjectSqliteStorage::new(host.clone()));
    let db = DurableObjectSqliteDatabase::new(storage.clone());
    (host, storage, db)
}

#[tokio::test]
async fn blob_and_bigint_bindings_round_trip_and_get_uses_only_next() {
    let (host, _, db) = database();
    db.exec("CREATE TABLE blobs (id INTEGER PRIMARY KEY,data BLOB,n INTEGER)")
        .await
        .unwrap();
    db.run(
        "INSERT INTO blobs VALUES (?,?,?)",
        &[1.into(), SqliteValue::Blob(vec![1, 2, 3]), 7.into()],
    )
    .await
    .unwrap();
    db.run(
        "INSERT INTO blobs VALUES (?,?,?)",
        &[2.into(), SqliteValue::Blob(vec![4, 5]), 8.into()],
    )
    .await
    .unwrap();
    let row = db
        .get("SELECT data,n FROM blobs ORDER BY id", &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row["data"], SqliteValue::Blob(vec![1, 2, 3]));
    assert_eq!(row["n"], SqliteValue::Real(7.0));
    assert_eq!(&*host.operations.lock(), &["next"]);
    assert_eq!(db.all("SELECT n FROM blobs", &[]).await.unwrap().len(), 2);
    assert_eq!(&*host.operations.lock(), &["next", "toArray"]);
    assert!(
        db.get("SELECT n FROM blobs WHERE id=9", &[])
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn unsafe_integer_binding_is_rejected_without_running_sql() {
    let (_, _, db) = database();
    db.exec("CREATE TABLE numbers (n INTEGER)").await.unwrap();
    for n in [9_007_199_254_740_993, -9_007_199_254_740_993] {
        assert_eq!(
            db.run("INSERT INTO numbers VALUES (?)", &[n.into()])
                .await
                .unwrap_err()
                .to_string(),
            format!("Durable Object SQL cannot bind {n} without losing precision")
        );
    }
    assert!(
        db.all("SELECT n FROM numbers", &[])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn idle_statement_runs_before_its_future_is_returned_and_close_preserves_host() {
    let (host, _, db) = database();
    db.exec("CREATE TABLE items (id INTEGER PRIMARY KEY)")
        .await
        .unwrap();
    let written = db.run("INSERT INTO items VALUES (?)", &[1.into()]);
    assert_eq!(
        host.connection
            .lock()
            .query_row("SELECT id FROM items", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    written.await.unwrap();
    db.close().await.unwrap();
    db.close().await.unwrap();
    assert!(db.get("SELECT id FROM items", &[]).await.unwrap().is_some());
}

#[tokio::test]
async fn rejected_transaction_rolls_back_and_retires_all_handle_operations() {
    let (_, _, db) = database();
    db.exec("CREATE TABLE items (id INTEGER PRIMARY KEY)")
        .await
        .unwrap();
    let (send, receive) = tokio::sync::oneshot::channel();
    let error = SqliteError::new("abort");
    let result = db
        .transaction::<(), _, _>({
            let error = error.clone();
            move |tx| async move {
                send.send(tx.clone()).ok().unwrap();
                tx.run("INSERT INTO items VALUES (?)", &[1.into()]).await?;
                Err(error)
            }
        })
        .await
        .unwrap_err();
    assert!(result.same_error(&error));
    assert!(
        db.all("SELECT id FROM items", &[])
            .await
            .unwrap()
            .is_empty()
    );
    let handle = receive.await.unwrap();
    assert!(
        handle
            .exec("SELECT 1")
            .await
            .unwrap_err()
            .to_string()
            .contains("no longer active")
    );
    assert!(handle.run("SELECT ?", &[1.into()]).await.is_err());
    assert!(handle.get("SELECT 1", &[]).await.is_err());
    assert!(handle.all("SELECT 1", &[]).await.is_err());
}

#[tokio::test]
async fn adapters_share_the_storage_queue_and_close_waits_for_transactions() {
    let (_, storage, db) = database();
    let other = DurableObjectSqliteDatabase::new(storage);
    db.exec("CREATE TABLE items (id INTEGER PRIMARY KEY)")
        .await
        .unwrap();
    let (started, entered) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let transaction = db.transaction::<(), _, _>(move |tx| async move {
        tx.run("INSERT INTO items VALUES (?)", &[1.into()]).await?;
        started.send(()).unwrap();
        gate.await.unwrap();
        Err(SqliteError::new("abort"))
    });
    entered.await.unwrap();
    let outside = other.run("INSERT INTO items VALUES (?)", &[2.into()]);
    let read = other.all("SELECT id FROM items", &[]);
    let closed = tokio::spawn(db.close());
    tokio::task::yield_now().await;
    assert!(!closed.is_finished());
    release.send(()).unwrap();
    assert!(transaction.await.is_err());
    outside.await.unwrap();
    let rows = read.await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], SqliteValue::Real(2.0));
    closed.await.unwrap().unwrap();
}

#[tokio::test]
async fn direct_storage_calls_participate_in_the_active_host_transaction() {
    let (host, storage, db) = database();
    db.exec("CREATE TABLE items (id INTEGER PRIMARY KEY)")
        .await
        .unwrap();
    let (started, entered) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let transaction = db.transaction::<(), _, _>(move |tx| async move {
        tx.run("INSERT INTO items VALUES (?)", &[1.into()]).await?;
        started.send(()).unwrap();
        gate.await.unwrap();
        Err(SqliteError::new("abort"))
    });
    entered.await.unwrap();
    storage
        .exec(
            "INSERT INTO items VALUES (?)",
            &[DurableObjectSqlValue::Number(2.0)],
        )
        .unwrap();
    release.send(()).unwrap();
    assert!(transaction.await.is_err());
    assert_eq!(
        host.connection
            .lock()
            .query_row("SELECT count(*) FROM items", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn portable_migrations_run_through_host_adapter() {
    let (_, _, db) = database();
    let migrations = [SqliteMigration {
        version: 1,
        statements: vec![
            "CREATE TABLE items (id INTEGER)".into(),
            "INSERT INTO items VALUES (7)".into(),
        ],
    }];
    apply_sqlite_migrations(&db, &migrations, "app_schema")
        .await
        .unwrap();
    apply_sqlite_migrations(&db, &migrations, "app_schema")
        .await
        .unwrap();
    assert_eq!(
        db.get("SELECT id FROM items", &[]).await.unwrap().unwrap()["id"],
        SqliteValue::Real(7.0)
    );
}

#[tokio::test]
async fn a_long_backlog_after_a_transaction_does_not_recurse_on_the_stack() {
    let (_, _, db) = database();
    db.exec("CREATE TABLE items (id INTEGER)").await.unwrap();
    let (release, gate) = tokio::sync::oneshot::channel();
    let transaction = db.transaction(move |_| async move {
        gate.await.unwrap();
        Ok(())
    });
    let writes: Vec<_> = (0..10000)
        .map(|id| db.run("INSERT INTO items VALUES (?)", &[id.into()]))
        .collect();
    release.send(()).unwrap();
    transaction.await.unwrap();
    for write in writes {
        write.await.unwrap();
    }
    assert_eq!(
        db.get("SELECT count(*) AS n FROM items", &[])
            .await
            .unwrap()
            .unwrap()["n"],
        SqliteValue::Real(10000.0)
    );
}
