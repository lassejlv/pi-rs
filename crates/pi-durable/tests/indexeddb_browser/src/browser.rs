//! Differential and semantic checks of the web adapter against real IndexedDB.
use pi_durable::storage::indexeddb::{
    Accept, Direction, IdbBackend, IdbKey, IdbKeyRange, IdbMode, IdbTransaction, Source,
    memory::MemoryIdbFactory,
    schema::{self, SCHEMA},
    web::WebIdbFactory,
};
use pi_durable::{Error, JsonString, JsonValue, MAX_SAFE_INTEGER, Result};
use wasm_bindgen::prelude::*;

fn field<'a>(row: &'a JsonValue, key: &str) -> Option<&'a JsonValue> {
    row.as_object()?.get(&JsonString::from(key))
}
fn parse(text: &str) -> JsonValue {
    pi_chord::json::parse(text).expect("test JSON")
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::message(e.to_string())
}
fn rows(stores: &JsonValue, store: &str) -> Vec<JsonValue> {
    stores[store]["rows"].as_array().cloned().unwrap_or_default()
}
fn check(ok: bool, what: impl FnOnce() -> String) -> Result<()> {
    if ok { Ok(()) } else { Err(Error::message(what())) }
}

#[wasm_bindgen]
pub async fn run(fixture: String) -> std::result::Result<String, JsValue> {
    suite(fixture).await.map_err(|e| JsValue::from_str(&e.to_string()))
}
#[wasm_bindgen]
pub async fn read_js_written(fixture: String) -> std::result::Result<String, JsValue> {
    js_written(fixture).await.map_err(|e| JsValue::from_str(&e.to_string()))
}

async fn js_written(fixture: String) -> Result<String> {
    let fixture = pi_chord::json::parse(&fixture).map_err(err)?;
    let stores = &fixture["finalStores"];
    let connection = WebIdbFactory::global()?.open("js-written").await?;
    let tx = connection.transaction(schema::ALL_STORES, IdbMode::ReadOnly)?;
    let mut compared = 0;
    for store in SCHEMA {
        let got = tx.values(Source::store(store.name), None, Direction::Next, usize::MAX, None).await?;
        let expected = rows(stores, store.name);
        check(got == expected, || format!("js-written {} differs: {} vs {} rows", store.name, got.len(), expected.len()))?;
        compared += got.len();
    }
    tx.finish().await?;
    connection.close();
    Ok(format!("{{\"jsWrittenRows\":{compared}}}"))
}

async fn suite(fixture: String) -> Result<String> {
    let fixture = pi_chord::json::parse(&fixture).map_err(err)?;
    let stores = &fixture["finalStores"];
    let web = WebIdbFactory::global()?;
    let _ = web.delete("rust-written").await;
    let connection = web.open("rust-written").await?;
    // Schema creation adds the initial meta row; the fixture replaces it.
    let tx = connection.transaction(schema::ALL_STORES, IdbMode::ReadWrite)?;
    for store in SCHEMA {
        for row in rows(stores, store.name) {
            tx.put(store.name, row).await?;
        }
    }
    tx.finish().await?;
    let memory = MemoryIdbFactory::new();
    for store in SCHEMA {
        memory.load("m", store.name, rows(stores, store.name))?;
    }
    let model = memory.open("m");
    let mut checks = 0usize;
    for store in SCHEMA {
        let w = connection.transaction(&[store.name], IdbMode::ReadOnly)?;
        let m = model.transaction(&[store.name], IdbMode::ReadOnly)?;
        let mut sources = vec![(Source::store(store.name), store.key_path)];
        sources.extend(store.indexes.iter().map(|index| (Source::index(store.name, index.name), index.key_path)));
        for (source, path) in sources {
            let keys: Vec<IdbKey> = rows(stores, store.name).iter().filter_map(|row| path.extract(row)).collect();
            let mut ranges: Vec<Option<IdbKeyRange>> = vec![None];
            for key in &keys {
                ranges.push(Some(IdbKeyRange::only(key.clone())));
                ranges.push(Some(IdbKeyRange::lower_bound(key.clone(), true)));
                ranges.push(Some(IdbKeyRange::upper_bound(key.clone(), false)));
                if let IdbKey::Array(parts) = key {
                    let mut low = parts[..1].to_vec();
                    low.push(IdbKey::Number(0.0));
                    let mut high = parts[..1].to_vec();
                    high.push(IdbKey::Number(MAX_SAFE_INTEGER as f64));
                    ranges.push(Some(IdbKeyRange::bound(IdbKey::Array(low), IdbKey::Array(high), false, true)));
                }
            }
            for range in ranges {
                for direction in [Direction::Next, Direction::Prev] {
                    for (limit, filtered) in [(usize::MAX, false), (2, false), (2, true)] {
                        let accept = || -> Option<Accept> {
                            filtered.then(|| -> Accept {
                                Box::new(|row: &JsonValue| {
                                    field(row, "id").or_else(|| field(row, "seq")).and_then(JsonValue::as_f64).is_some_and(|id| id as i64 % 2 == 0)
                                })
                            })
                        };
                        let got = w.values(source, range.clone(), direction, limit, accept()).await?;
                        let want = m.values(source, range.clone(), direction, limit, accept()).await?;
                        check(got == want, || format!("{source:?} {range:?} {direction:?} {limit}: browser {} rows, model {} rows", got.len(), want.len()))?;
                        checks += 1;
                    }
                }
            }
            for key in keys {
                let (got, want) = match source.index {
                    None => (w.get(store.name, key.clone()).await?, m.get(store.name, key.clone()).await?),
                    Some(index) => (w.index_get(store.name, index, key.clone()).await?, m.index_get(store.name, index, key.clone()).await?),
                };
                check(got == want && got.is_some(), || format!("{source:?} get {key:?}"))?;
                checks += 1;
            }
        }
        w.finish().await?;
        m.finish().await?;
    }

    // One readwrite transaction stays active across many awaited requests.
    let tx = connection.transaction(&[schema::RECORD_IDS], IdbMode::ReadWrite)?;
    for id in 0..300 {
        tx.put(schema::RECORD_IDS, parse(&format!(r#"{{"id":{},"recordType":"bulk"}}"#, 1_000_000 + id))).await?;
    }
    tx.finish().await?;
    let tx = connection.transaction(&[schema::RECORD_IDS], IdbMode::ReadOnly)?;
    let bulk = tx.values(Source::store(schema::RECORD_IDS), Some(IdbKeyRange::lower_bound(IdbKey::Number(1_000_000.0), false)), Direction::Next, usize::MAX, None).await?;
    tx.finish().await?;
    check(bulk.len() == 300, || format!("bulk rows {}", bulk.len()))?;

    // Abort discards; a duplicate add fails and aborts; readonly rejects writes.
    let row = parse(r#"{"id":2000000,"recordType":"aborted"}"#);
    let tx = connection.transaction(&[schema::RECORD_IDS], IdbMode::ReadWrite)?;
    tx.add(schema::RECORD_IDS, row.clone()).await?;
    tx.abort().await?;
    let tx = connection.transaction(&[schema::RECORD_IDS], IdbMode::ReadWrite)?;
    check(tx.get(schema::RECORD_IDS, IdbKey::Number(2_000_000.0)).await?.is_none(), || "abort kept a row".into())?;
    let duplicate = tx.add(schema::RECORD_IDS, parse(r#"{"id":1000000,"recordType":"dup"}"#)).await;
    let duplicate = duplicate.err().map(|e| e.to_string()).unwrap_or_default();
    check(duplicate.starts_with("ConstraintError"), || format!("duplicate add: {duplicate}"))?;
    let finished = tx.finish().await;
    check(finished.is_err(), || "failed request did not abort".into())?;
    let tx = connection.transaction(&[schema::RECORD_IDS], IdbMode::ReadOnly)?;
    let readonly = tx.put(schema::RECORD_IDS, row.clone()).await.err().map(|e| e.to_string()).unwrap_or_default();
    check(readonly.starts_with("ReadOnlyError"), || format!("readonly put: {readonly}"))?;
    let _ = tx.finish().await;

    // Creation order: an earlier reader misses a later write, a later reader sees it.
    let before = connection.transaction(&[schema::RECORD_IDS], IdbMode::ReadOnly)?;
    let write = connection.transaction(&[schema::RECORD_IDS], IdbMode::ReadWrite)?;
    let after = connection.transaction(&[schema::RECORD_IDS], IdbMode::ReadOnly)?;
    let key = IdbKey::Number(3_000_000.0);
    let ordered = parse(r#"{"id":3000000,"recordType":"ordered"}"#);
    let (seen_before, written, seen_after) = futures::join!(
        async { before.get(schema::RECORD_IDS, key.clone()).await },
        async { write.put(schema::RECORD_IDS, ordered.clone()).await },
        async { after.get(schema::RECORD_IDS, key.clone()).await },
    );
    written?;
    check(seen_before?.is_none(), || "earlier reader saw a later write".into())?;
    check(seen_after? == Some(ordered), || "later reader missed an earlier write".into())?;
    before.finish().await?;
    write.finish().await?;
    after.finish().await?;

    // UTF-16 and prototype-like keys survive the structured clone.
    let odd = parse(r#"{"id":4000000,"recordType":"\ud800x\udfff","__proto__":{"constructor":"\u0000"},"nested":[1.5,null,true,"😀"]}"#);
    let tx = connection.transaction(&[schema::RECORD_IDS], IdbMode::ReadWrite)?;
    tx.put(schema::RECORD_IDS, odd.clone()).await?;
    let back = tx.get(schema::RECORD_IDS, IdbKey::Number(4_000_000.0)).await?;
    tx.finish().await?;
    check(back == Some(odd), || format!("odd row: {back:?}"))?;
    // Leave the database with exactly the fixture rows for the page to dump.
    let tx = connection.transaction(&[schema::RECORD_IDS], IdbMode::ReadWrite)?;
    tx.delete(schema::RECORD_IDS, IdbKeyRange::lower_bound(IdbKey::Number(1_000_000.0), false)).await?;
    tx.finish().await?;
    connection.close();
    Ok(format!("{{\"differentialChecks\":{checks}}}"))
}
