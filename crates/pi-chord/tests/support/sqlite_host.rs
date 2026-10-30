use parking_lot::Mutex;
use pi_chord::sqlite::{cloudflare::*, *};
use rusqlite::{
    Connection,
    types::{Value, ValueRef},
};
use std::{collections::VecDeque, sync::Arc};

pub struct Host {
    pub connection: Arc<Mutex<Connection>>,
    pub operations: Arc<Mutex<Vec<&'static str>>>,
}
impl Host {
    pub fn new() -> Self {
        Self {
            connection: Arc::new(Mutex::new(Connection::open_in_memory().unwrap())),
            operations: Arc::default(),
        }
    }
}
struct Cursor {
    rows: VecDeque<DurableObjectSqlRow>,
    operations: Arc<Mutex<Vec<&'static str>>>,
}
impl DurableObjectSqlCursor for Cursor {
    fn next(&mut self) -> SqliteResult<Option<DurableObjectSqlRow>> {
        self.operations.lock().push("next");
        Ok(self.rows.pop_front())
    }
    fn into_array(self: Box<Self>) -> SqliteResult<Vec<DurableObjectSqlRow>> {
        self.operations.lock().push("toArray");
        Ok(self.rows.into())
    }
}
impl DurableObjectSqliteHost for Host {
    fn exec(
        &self,
        sql: &str,
        bindings: &[DurableObjectSqlValue],
    ) -> SqliteResult<Box<dyn DurableObjectSqlCursor>> {
        let connection = self.connection.lock();
        let mut result = VecDeque::new();
        if bindings.is_empty() && sql.trim().trim_end_matches(';').contains(';') {
            connection.execute_batch(sql)?;
        } else {
            let mut statement = connection.prepare(sql)?;
            let bindings: Vec<_> = bindings
                .iter()
                .map(|value| match value {
                    DurableObjectSqlValue::Null => Value::Null,
                    DurableObjectSqlValue::Number(n) => Value::Real(*n),
                    DurableObjectSqlValue::Text(s) => Value::Text(s.clone()),
                    DurableObjectSqlValue::Blob(b) => Value::Blob(b.clone()),
                })
                .collect();
            if statement.column_count() == 0 {
                statement.execute(rusqlite::params_from_iter(bindings))?;
            } else {
                let names: Vec<_> = statement.column_names().map(str::to_owned).collect();
                let mut cursor = statement.query(rusqlite::params_from_iter(bindings))?;
                while let Some(row) = cursor.next()? {
                    let mut value = DurableObjectSqlRow::new();
                    for (i, name) in names.iter().enumerate() {
                        value.insert(
                            name.clone(),
                            match row.get_ref(i)? {
                                ValueRef::Null => DurableObjectSqlValue::Null,
                                ValueRef::Integer(n) => DurableObjectSqlValue::Number(n as f64),
                                ValueRef::Real(n) => DurableObjectSqlValue::Number(n),
                                ValueRef::Text(s) => DurableObjectSqlValue::Text(
                                    String::from_utf8_lossy(s).into_owned(),
                                ),
                                ValueRef::Blob(b) => DurableObjectSqlValue::Blob(b.to_vec()),
                            },
                        );
                    }
                    result.push_back(value);
                }
            }
        }
        Ok(Box::new(Cursor {
            rows: result,
            operations: self.operations.clone(),
        }))
    }
    fn transaction_raw(
        &self,
        callback: DurableObjectTransactionCallback,
    ) -> SqliteFuture<SqliteTransactionResult> {
        let connection = self.connection.clone();
        Box::pin(async move {
            connection.lock().execute_batch("BEGIN")?;
            match callback().await {
                Ok(value) => {
                    connection.lock().execute_batch("COMMIT")?;
                    Ok(value)
                }
                Err(error) => match connection.lock().execute_batch("ROLLBACK") {
                    Ok(()) => Err(error),
                    Err(rollback) => Err(SqliteError::rollback(error, rollback.into())),
                },
            }
        })
    }
}
