//! SQLite connections backing `node:sqlite` (`js/node/sqlite.js`) on rusqlite (bundled SQLite).
//!
//! Connections live in a per-thread table keyed by id; statements are prepared through the
//! connection's statement cache, so a `StatementSync` only keeps its SQL text. Values follow
//! Node 22: numbers bind as REAL, bigints as INTEGER, strings as TEXT, typed arrays as BLOB;
//! INTEGER results are numbers (bigints with `readBigInts`), BLOBs are `Uint8Array`s, and rows
//! are null-prototype objects.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use crate::js::{JsArrayBuffer, JsTypedArray};
use crate::js::{Context, JsArgs, JsBigInt, JsObject, JsResult, JsString, JsValue, js_string};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags};

use super::{array, bytes_to_js, coded_error, object};
use crate::engine::{OpFn, arg_f64, arg_string, js_str};

thread_local! {
    static DATABASES: RefCell<HashMap<u64, Connection>> = RefCell::new(HashMap::new());
    static NEXT_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

fn sqlite_error(ctx: &mut Context, e: &rusqlite::Error) -> crate::js::JsError {
    let (errcode, errstr) = match e {
        rusqlite::Error::SqliteFailure(f, _) => (
            f.extended_code,
            rusqlite::ffi::code_to_str(f.extended_code).to_string(),
        ),
        _ => (1, "SQL logic error".to_string()),
    };
    let message = match e {
        rusqlite::Error::SqliteFailure(_, Some(m)) => m.clone(),
        other => other.to_string(),
    };
    coded_error(
        ctx,
        "Error",
        &message,
        &[
            ("code", js_str("ERR_SQLITE_ERROR")),
            ("errcode", JsValue::from(errcode)),
            ("errstr", js_str(&errstr)),
        ],
    )
}

fn invalid_state(ctx: &mut Context, message: &str) -> crate::js::JsError {
    coded_error(
        ctx,
        "Error",
        message,
        &[("code", js_str("ERR_INVALID_STATE"))],
    )
}

fn with_db<R>(
    ctx: &mut Context,
    id: u64,
    f: impl FnOnce(&Connection) -> Result<R, rusqlite::Error>,
) -> JsResult<R> {
    let r = DATABASES.with(|dbs| dbs.borrow().get(&id).map(f));
    match r {
        None => Err(invalid_state(ctx, "database is not open")),
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => Err(sqlite_error(ctx, &e)),
    }
}

fn db_id(args: &[JsValue], ctx: &mut Context) -> JsResult<u64> {
    Ok(arg_f64(args, 0, ctx)? as u64)
}

/// `ops.sqliteOpen(path, readOnly, timeoutMs, foreignKeys, doubleQuotedStrings) -> id`
fn op_open(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let path = arg_string(args, 0, ctx)?;
    let read_only = args.get_or_undefined(1).to_boolean();
    let timeout = arg_f64(args, 2, ctx)?;
    let foreign_keys = args.get_or_undefined(3).to_boolean();
    let dqs = args.get_or_undefined(4).to_boolean();
    let flags = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
    };
    let opened = Connection::open_with_flags(&path, flags).and_then(|conn| {
        conn.busy_timeout(Duration::from_millis(
            if timeout.is_finite() && timeout > 0.0 {
                timeout as u64
            } else {
                0
            },
        ))?;
        conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DQS_DML, dqs)?;
        conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DQS_DDL, dqs)?;
        conn.pragma_update(None, "foreign_keys", foreign_keys)?;
        Ok(conn)
    });
    let conn = opened.map_err(|e| sqlite_error(ctx, &e))?;
    let id = NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    });
    DATABASES.with(|dbs| dbs.borrow_mut().insert(id, conn));
    Ok(JsValue::from(id as f64))
}

/// `ops.sqliteClose(id)`
fn op_close(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = db_id(args, ctx)?;
    let conn = DATABASES.with(|dbs| dbs.borrow_mut().remove(&id));
    match conn {
        None => Err(invalid_state(ctx, "database is not open")),
        Some(conn) => match conn.close() {
            Ok(()) => Ok(JsValue::undefined()),
            Err((_, e)) => Err(sqlite_error(ctx, &e)),
        },
    }
}

/// `ops.sqliteExec(id, sql)`: run every statement of `sql`.
fn op_exec(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = db_id(args, ctx)?;
    let sql = arg_string(args, 1, ctx)?;
    with_db(ctx, id, |c| c.execute_batch(&sql))?;
    Ok(JsValue::undefined())
}

/// `ops.sqliteInTransaction(id)`
fn op_in_transaction(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = db_id(args, ctx)?;
    Ok(JsValue::from(with_db(ctx, id, |c| Ok(!c.is_autocommit()))?))
}

/// `ops.sqlitePrepare(id, sql) -> [parameterNames, columns]`: compile once to report errors at
/// prepare time, and describe the statement (`columns()` entries are `[name, column, table,
/// database, type]`).
fn op_prepare(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = db_id(args, ctx)?;
    let sql = arg_string(args, 1, ctx)?;
    type Column = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let (names, columns): (Vec<Option<String>>, Vec<Column>) = with_db(ctx, id, |c| {
        let stmt = c.prepare_cached(&sql)?;
        let names = (1..=stmt.parameter_count())
            .map(|i| stmt.parameter_name(i).map(str::to_string))
            .collect();
        let columns = stmt
            .column_names()
            .map(|name| (name.to_string(), None, None, None, None))
            .collect();
        Ok((names, columns))
    })?;
    let names: Vec<JsValue> = names
        .iter()
        .map(|n| n.as_deref().map_or(JsValue::null(), js_str))
        .collect();
    let names = array(ctx, names);
    let opt = |v: &Option<String>| v.as_deref().map_or(JsValue::null(), js_str);
    let mut cols = Vec::with_capacity(columns.len());
    for (name, column, table, database, ty) in &columns {
        let entry = vec![
            js_str(name),
            opt(column),
            opt(table),
            opt(database),
            opt(ty),
        ];
        cols.push(array(ctx, entry));
    }
    let cols = array(ctx, cols);
    Ok(array(ctx, vec![names, cols]))
}

/// Bytes of an `ArrayBufferView` (a BLOB value), if `v` is one.
fn view_bytes(v: &JsValue, ctx: &mut Context) -> JsResult<Option<Vec<u8>>> {
    let Some(obj) = v.as_object() else {
        return Ok(None);
    };
    if JsTypedArray::from_object(obj.clone()).is_ok()
        || crate::js::JsDataView::from_object(obj.clone()).is_ok()
    {
        return super::bytes_arg(v, ctx).map(Some);
    }
    if JsArrayBuffer::from_object(obj.clone()).is_ok() {
        return Ok(None);
    }
    Ok(None)
}

fn to_sql(v: &JsValue, index: &str, ctx: &mut Context) -> JsResult<SqlValue> {
    if v.is_null() {
        return Ok(SqlValue::Null);
    }
    if let Some(n) = v.as_number() {
        return Ok(SqlValue::Real(n));
    }
    if let Some(s) = v.as_string() {
        return Ok(SqlValue::Text(s.to_std_string_lossy()));
    }
    if let Some(b) = v.as_bigint() {
        return match b.to_string().parse::<i64>() {
            Ok(i) => Ok(SqlValue::Integer(i)),
            Err(_) => Err(coded_error(
                ctx,
                "RangeError",
                &format!("BigInt value is too large to bind to parameter {index}"),
                &[("code", js_str("ERR_INVALID_ARG_VALUE"))],
            )),
        };
    }
    if let Some(bytes) = view_bytes(v, ctx)? {
        return Ok(SqlValue::Blob(bytes));
    }
    Err(coded_error(
        ctx,
        "TypeError",
        &format!("Provided value cannot be bound to SQLite parameter {index}."),
        &[("code", js_str("ERR_INVALID_ARG_TYPE"))],
    ))
}

/// Collect `(position, value)` bindings from positional values and an optional named object.
fn bindings(
    ctx: &mut Context,
    names: &[Option<String>],
    positional: &JsValue,
    named: &JsValue,
    bare: bool,
) -> JsResult<Vec<(usize, SqlValue)>> {
    let mut out = Vec::new();
    if let Some(obj) = named.as_object() {
        for (i, name) in names.iter().enumerate() {
            let Some(name) = name else { continue };
            let mut v = obj.get(JsString::from(name.as_str()), ctx)?;
            if v.is_undefined()
                && bare
                && let Some(stripped) = name.get(1..)
            {
                if obj.has_own_property(JsString::from(stripped), ctx)? {
                    v = obj.get(JsString::from(stripped), ctx)?;
                } else {
                    continue;
                }
            } else if v.is_undefined()
                && !obj.has_own_property(JsString::from(name.as_str()), ctx)?
            {
                continue;
            }
            out.push((i + 1, to_sql(&v, name, ctx)?));
        }
    }
    if let Some(list) = positional.as_object() {
        let len = list.get(js_string!("length"), ctx)?.to_length(ctx)? as usize;
        // Positional values fill the anonymous (`?`) parameters in order.
        let mut slots = names
            .iter()
            .enumerate()
            .filter(|(_, n)| n.as_deref().is_none_or(|n| n.starts_with('?')))
            .map(|(i, _)| i + 1);
        for k in 0..len {
            let v = list.get(k, ctx)?;
            let Some(slot) = slots.next() else {
                return Err(coded_error(
                    ctx,
                    "Error",
                    "column index out of range",
                    &[
                        ("code", js_str("ERR_SQLITE_ERROR")),
                        ("errcode", JsValue::from(25)),
                        ("errstr", js_str("column index out of range")),
                    ],
                ));
            };
            out.push((slot, to_sql(&v, &(k + 1).to_string(), ctx)?));
        }
    }
    Ok(out)
}

fn statement_names(conn: &Connection, sql: &str) -> Result<Vec<Option<String>>, rusqlite::Error> {
    let stmt = conn.prepare_cached(sql)?;
    Ok((1..=stmt.parameter_count())
        .map(|i| stmt.parameter_name(i).map(str::to_string))
        .collect())
}

/// `ops.sqliteRun(id, sql, positional, named, bareNames) -> [changes, lastInsertRowid]`
fn op_run(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = db_id(args, ctx)?;
    let sql = arg_string(args, 1, ctx)?;
    let names = with_db(ctx, id, |c| statement_names(c, &sql))?;
    let binds = bindings(
        ctx,
        &names,
        args.get_or_undefined(2),
        args.get_or_undefined(3),
        args.get_or_undefined(4).to_boolean(),
    )?;
    let (changes, rowid) = with_db(ctx, id, |c| {
        let mut stmt = c.prepare_cached(&sql)?;
        stmt.clear_bindings();
        for (i, v) in &binds {
            stmt.raw_bind_parameter(*i, v)?;
        }
        stmt.raw_execute()?;
        Ok((c.changes(), c.last_insert_rowid()))
    })?;
    let changes = JsValue::from(changes as f64);
    let rowid = JsValue::from(rowid as f64);
    Ok(array(ctx, vec![changes, rowid]))
}

enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

fn cell(v: ValueRef<'_>) -> Cell {
    match v {
        ValueRef::Null => Cell::Null,
        ValueRef::Integer(i) => Cell::Integer(i),
        ValueRef::Real(f) => Cell::Real(f),
        ValueRef::Text(t) => Cell::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Cell::Blob(b.to_vec()),
    }
}

/// `ops.sqliteQuery(id, sql, positional, named, bareNames, all, readBigInts) -> rows | row | undefined`
fn op_query(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = db_id(args, ctx)?;
    let sql = arg_string(args, 1, ctx)?;
    let all = args.get_or_undefined(5).to_boolean();
    let big = args.get_or_undefined(6).to_boolean();
    let names = with_db(ctx, id, |c| statement_names(c, &sql))?;
    let binds = bindings(
        ctx,
        &names,
        args.get_or_undefined(2),
        args.get_or_undefined(3),
        args.get_or_undefined(4).to_boolean(),
    )?;
    let (columns, rows) = with_db(ctx, id, |c| {
        let mut stmt = c.prepare_cached(&sql)?;
        stmt.clear_bindings();
        for (i, v) in &binds {
            stmt.raw_bind_parameter(*i, v)?;
        }
        let columns: Vec<String> = stmt.column_names().map(str::to_string).collect();
        let n = columns.len();
        let mut rows = Vec::new();
        let mut q = stmt.raw_query();
        while let Some(row) = q.next()? {
            let mut cells = Vec::with_capacity(n);
            for i in 0..n {
                cells.push(cell(row.get_ref(i)?));
            }
            rows.push(cells);
            if !all {
                break;
            }
        }
        Ok((columns, rows))
    })?;
    let mut out = Vec::with_capacity(rows.len());
    for cells in rows {
        let row = JsObject::with_null_proto();
        for (name, c) in columns.iter().zip(cells) {
            let v = match c {
                Cell::Null => JsValue::null(),
                Cell::Integer(i) if big => JsValue::from(JsBigInt::from(i)),
                Cell::Integer(i) => {
                    if i.unsigned_abs() > 9_007_199_254_740_991 {
                        return Err(coded_error(
                            ctx,
                            "RangeError",
                            &format!(
                                "Value is too large to be represented as a JavaScript number: {i}"
                            ),
                            &[("code", js_str("ERR_OUT_OF_RANGE"))],
                        ));
                    }
                    JsValue::from(i as f64)
                }
                Cell::Real(f) => JsValue::from(f),
                Cell::Text(t) => js_str(&t),
                Cell::Blob(b) => bytes_to_js(&b, ctx)?,
            };
            row.create_data_property_or_throw(JsString::from(name.as_str()), v, ctx)?;
        }
        out.push(JsValue::from(row));
    }
    if all {
        Ok(array(ctx, out))
    } else {
        Ok(out.into_iter().next().unwrap_or_default())
    }
}

/// `ops.sqliteInfo(id) -> {open}` (diagnostics for tests).
fn op_info(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = db_id(args, ctx)?;
    let open = DATABASES.with(|dbs| dbs.borrow().contains_key(&id));
    object(ctx, vec![("open", JsValue::from(open))])
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("sqliteOpen", 5, op_open),
    ("sqliteClose", 1, op_close),
    ("sqliteExec", 2, op_exec),
    ("sqliteInTransaction", 1, op_in_transaction),
    ("sqlitePrepare", 2, op_prepare),
    ("sqliteRun", 5, op_run),
    ("sqliteQuery", 7, op_query),
    ("sqliteInfo", 1, op_info),
];

/// Close every connection of this runtime.
pub(crate) fn teardown(_ctx: &mut Context) {
    DATABASES.with(|dbs| dbs.borrow_mut().clear());
}
