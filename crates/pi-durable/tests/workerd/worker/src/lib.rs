//! The Rust Durable Object under test, compiled to wasm and driven from `worker.mjs`.
//! The JS class owns the Durable Object lifecycle; every behaviour under test runs here.
//! `storage.test.ts` runs the storage conformance cases and the schema migration on the
//! object's own SQLite through the JavaScript-backed adapter. The Harness wake scenarios
//! (`wake.rs`, feature `wake`, class `WakeSession` in worker.mjs) run a Harness on the object.

use pi_durable::{
    storage::sqlite::{
        SqliteExecutor, SqliteStorageOptions, SqliteValue, apply_sqlite_migrations_for_schema,
        cloudflare_js::{js_durable_object_sqlite_database, open_js_durable_object_sqlite_storage},
        create_sqlite_migrations, create_sqlite_schema,
    },
    testing::{StorageBackend, create_storage_conformance_for},
    worker::{Wake, alarm_at, arm, arm_fallback, js::JsStorage},
};
use std::sync::Arc;
use wasm_bindgen::prelude::*;

#[cfg(feature = "wake")]
mod wake;
#[cfg(feature = "harness")]
mod harness_turn;

fn fail(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}

/// Opens the object's durable storage; the constructor of the Durable Object calls it.
#[wasm_bindgen]
pub struct Core {
    _storage: Arc<dyn pi_durable::storage::Storage>,
    host: JsStorage,
}
#[wasm_bindgen]
impl Core {
    pub async fn open(storage: JsValue) -> Result<Core, JsValue> {
        let host = JsStorage::new(storage.clone());
        let sql = open_js_durable_object_sqlite_storage(
            storage,
            SqliteStorageOptions {
                prefix: Some("core_".into()),
            },
        )
            .await
            .map_err(fail)?;
        Ok(Core {
            _storage: Arc::new(sql),
            host,
        })
    }
    /// Arm the object's alarm for a wake, as `WakeSession` does: the earliest sleep or a heartbeat.
    pub async fn arm(&self, active: bool, at: Option<f64>, now: f64, offset: f64) -> Result<(), JsValue> {
        arm(&self.host, &Wake { active, at }, now, offset).await.map_err(fail)
    }
    /// The fallback alarm armed before anything can fail; an earlier alarm is kept.
    #[wasm_bindgen(js_name = armFallback)]
    pub async fn arm_fallback(&self, now: f64) -> Result<(), JsValue> {
        arm_fallback(&self.host, now).await.map_err(fail)
    }
}

/// `alarm_at` for a wake of `{ active, at }`; `undefined` when no alarm is needed.
#[wasm_bindgen(js_name = alarmAt)]
pub fn alarm_for(active: bool, at: Option<f64>, now: f64, offset: f64) -> Option<f64> {
    alarm_at(&Wake { active, at }, now, offset)
}

/// The names of the storage conformance cases recorded for SQLite.
#[wasm_bindgen(js_name = conformanceCases)]
pub fn conformance_cases() -> Result<Vec<String>, JsValue> {
    Ok(create_storage_conformance_for(StorageBackend::Sqlite)
        .map_err(fail)?
        .iter()
        .map(|case| case.name.to_string_lossy().to_string())
        .collect())
}

/// Run storage conformance case `index` on `storage` under its own table prefix.
#[wasm_bindgen(js_name = runConformanceCase)]
pub async fn run_conformance_case(
    index: usize,
    storage: JsValue,
    prefix: String,
) -> Result<(), JsValue> {
    let cases = create_storage_conformance_for(StorageBackend::Sqlite).map_err(fail)?;
    let sql = open_js_durable_object_sqlite_storage(
        storage,
        SqliteStorageOptions {
            prefix: Some(prefix),
        },
    )
    .await
    .map_err(fail)?;
    cases[index].run(Arc::new(sql)).await.map_err(fail)
}

/// Migrate a version 1 schema to admit sleeping tasks, keeping tasks and indexes.
#[wasm_bindgen(js_name = migrationScenario)]
pub async fn migration_scenario(storage: JsValue) -> Result<(), JsValue> {
    let check = |ok: bool, what: &str| ok.then_some(()).ok_or_else(|| fail(format!("Failed: {what}")));
    let database = js_durable_object_sqlite_database(storage.clone());
    let schema = create_sqlite_schema(Some("legacy_")).map_err(fail)?;
    let tasks = schema.tables.tasks.clone();
    let insert = |id: i64, status: &'static str| {
        let sql = format!(
            "INSERT INTO {tasks} (id, conversation_id, kind, status, abort_requested, background, record) VALUES (?, 1, 'test', ?, 0, 0, '{{}}')"
        );
        let database = database.clone();
        async move { database.run(&sql, &[id.into(), status.into()]).await }
    };
    // A database written by a version that knew schema version 1 only.
    let migrations = create_sqlite_migrations(&schema);
    apply_sqlite_migrations_for_schema(&database, &schema, Some(&migrations[..1]))
        .await
        .map_err(fail)?;
    insert(5, "waiting").await.map_err(fail)?;
    check(insert(6, "sleeping").await.is_err(), "version 1 rejects sleeping")?;

    open_js_durable_object_sqlite_storage(
        storage,
        SqliteStorageOptions {
            prefix: Some("legacy_".into()),
        },
    )
    .await
    .map_err(fail)?;
    let version = database
        .get(&format!("SELECT version FROM {}", schema.tables.durable_schema), &[])
        .await
        .map_err(fail)?
        .ok_or_else(|| fail("No schema version"))?;
    check(version["version"] == SqliteValue::Real(2.0), "schema version 2")?;
    let status = database
        .get(&format!("SELECT status FROM {tasks} WHERE id = 5"), &[])
        .await
        .map_err(fail)?
        .ok_or_else(|| fail("Task 5 lost"))?;
    check(status["status"] == SqliteValue::Text("waiting".into()), "task kept")?;
    insert(6, "sleeping").await.map_err(fail)?;
    let indexes = database
        .all(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ? ORDER BY name",
            &[tasks.as_str().into()],
        )
        .await
        .map_err(fail)?;
    let mut expected = vec![
        schema.indexes.tasks_by_abort_requested.clone(),
        schema.indexes.tasks_by_background.clone(),
        schema.indexes.tasks_by_conversation.clone(),
        schema.indexes.tasks_by_kind.clone(),
        schema.indexes.tasks_by_status.clone(),
    ];
    expected.sort();
    let names: Vec<String> = indexes
        .iter()
        .filter_map(|row| match &row["name"] {
            SqliteValue::Text(name) => Some(name.clone()),
            _ => None,
        })
        .collect();
    check(names == expected, "indexes kept")
}
