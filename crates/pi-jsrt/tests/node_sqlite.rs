//! `node:sqlite` (DatabaseSync / StatementSync) against Node 22's observable behavior.

use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::json;

async fn run(src: &str) -> serde_json::Value {
    let dir = tempfile::tempdir().unwrap();
    let rt = JsRuntime::new(RuntimeOptions {
        cwd: Some(dir.path().to_path_buf()),
        ..Default::default()
    })
    .unwrap();
    let ns = rt.eval_module(src, "main.mjs").await.unwrap();
    let out = ns
        .call_method("run", vec![json!(dir.path().display().to_string())])
        .await
        .unwrap();
    drop(dir);
    out
}

#[tokio::test]
async fn database_sync_round_trips_values_and_rows() {
    let out = run(r#"
        import { DatabaseSync, StatementSync } from "node:sqlite";
        export function run() {
            const db = new DatabaseSync(":memory:");
            db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL, data BLOB, big INTEGER)");
            const insert = db.prepare("INSERT INTO t (name, score, data, big) VALUES (?, ?, ?, ?)");
            const first = insert.run("a", 1.5, new Uint8Array([1, 2, 3]), 9007199254740993n);
            insert.run("b", 2, null, 7n);
            const row = db.prepare("SELECT id, name, score, data, typeof(score) AS kind FROM t WHERE name = ?").get("a");
            const missing = db.prepare("SELECT 1 AS one FROM t WHERE name = ?").get("zzz");
            const all = db.prepare("SELECT name FROM t ORDER BY id").all();
            const named = db.prepare("SELECT name FROM t WHERE id = $id").get({ $id: 2 });
            const bare = db.prepare("SELECT name FROM t WHERE id = :id").get({ id: 1 });
            const bigStmt = db.prepare("SELECT big FROM t WHERE id = 1");
            let overflow;
            try { bigStmt.get(); } catch (e) { overflow = e.code; }
            bigStmt.setReadBigInts(true);
            const big = bigStmt.get().big;
            let syntax;
            try { db.prepare("SELEC 1"); } catch (e) { syntax = [e.code, typeof e.errcode, /syntax error/.test(e.message)]; }
            db.exec("BEGIN");
            const inTx = db.isTransaction;
            db.exec("ROLLBACK");
            const columns = db.prepare("SELECT name AS label FROM t").columns().map((c) => c.name);
            db.close();
            let closed;
            try { db.exec("SELECT 1"); } catch (e) { closed = e.code; }
            return {
                first: { changes: first.changes, lastInsertRowid: first.lastInsertRowid },
                proto: Object.getPrototypeOf(row) === null,
                row: { ...row, data: Array.from(row.data), isBytes: row.data instanceof Uint8Array },
                missing: missing === undefined,
                all: all.map((r) => r.name),
                named: named.name,
                bare: bare.name,
                overflow,
                big: typeof big + ":" + String(big),
                syntax,
                inTx,
                columns,
                closed,
                open: db.isOpen,
                statement: insert instanceof StatementSync,
            };
        }
    "#)
    .await;
    assert_eq!(
        out,
        json!({
            "first": {"changes": 1, "lastInsertRowid": 1},
            "proto": true,
            "row": {"id": 1, "name": "a", "score": 1.5, "data": [1, 2, 3], "kind": "real", "isBytes": true},
            "missing": true,
            "all": ["a", "b"],
            "named": "b",
            "bare": "a",
            "overflow": "ERR_OUT_OF_RANGE",
            "big": "bigint:9007199254740993",
            "syntax": ["ERR_SQLITE_ERROR", "number", true],
            "inTx": true,
            "columns": ["label"],
            "closed": "ERR_INVALID_STATE",
            "open": false,
            "statement": true,
        })
    );
}

#[tokio::test]
async fn database_sync_files_read_only_and_foreign_keys() {
    let out = run(r#"
        import { DatabaseSync } from "node:sqlite";
        import { join } from "node:path";
        export function run(dir) {
            const path = join(dir, "x.db");
            const db = new DatabaseSync(path, { timeout: 50 });
            db.exec("CREATE TABLE p (id INTEGER PRIMARY KEY); CREATE TABLE c (p INTEGER REFERENCES p(id)); INSERT INTO p VALUES (1);");
            let fk;
            try { db.prepare("INSERT INTO c VALUES (?)").run(5); } catch (e) { fk = /FOREIGN KEY/.test(e.message); }
            db.close();
            const ro = new DatabaseSync(path, { readOnly: true });
            const count = ro.prepare("SELECT count(*) AS n FROM p").get().n;
            let readOnly;
            try { ro.exec("INSERT INTO p VALUES (2)"); } catch (e) { readOnly = /readonly/.test(e.message); }
            ro.close();
            const lazy = new DatabaseSync(path, { open: false });
            const before = lazy.isOpen;
            lazy.open();
            const after = lazy.isOpen;
            lazy.close();
            let dqs;
            const strict = new DatabaseSync(":memory:");
            try { strict.prepare('SELECT "nope"').get(); } catch (e) { dqs = /no such column/.test(e.message); }
            strict.close();
            return { fk, count, readOnly, before, after, dqs };
        }
    "#)
    .await;
    assert_eq!(
        out,
        json!({"fk": true, "count": 1, "readOnly": true, "before": false, "after": true, "dqs": true})
    );
}

#[tokio::test]
async fn sqlite_is_only_a_node_scheme_builtin() {
    let dir = tempfile::tempdir().unwrap();
    let rt = JsRuntime::new(RuntimeOptions {
        cwd: Some(dir.path().to_path_buf()),
        ..Default::default()
    })
    .unwrap();
    let err = rt
        .eval_module("import 'sqlite';", "main.mjs")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("sqlite"), "{err}");
}
