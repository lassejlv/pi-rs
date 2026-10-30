//! A Session over IndexedDbStorage in the browser: the Session executor's
//! microtask run loop, serialized commits, snapshots and reopen.
use crate::context;
use pi_durable::{
    storage::{Storage, indexeddb::{IndexedDbStorage, web::WebIdbFactory}},
    *,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use wasm_bindgen::prelude::*;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Counter {
    count: u32,
}
impl DocumentState for Counter {}
fn counter() -> SessionDocToken<Counter> {
    define_doc(DocDefinition::new("counter", 1, SessionScope, || Ok(Counter { count: 0 }), |_, _, _| false))
        .unwrap()
}
fn session(storage: Arc<dyn Storage>) -> Session {
    Session::new(storage, SessionOptions { report: Arc::new(|_| {}), ..Default::default() })
}
async fn bump(session: &Session) -> Result<()> {
    session
        .commit(
            |tx| async move {
                tx.doc(&counter(), ()).await?.update(|c| {
                    c.count += 1;
                    Ok(())
                })
            },
            &context(),
        )
        .await
}
async fn count(session: &Session) -> Result<u32> {
    Ok(session.snapshot(&counter(), (), &context()).await?.map_or(0, |c| c.count))
}
fn step(what: &str) {
    web_sys::console::log_1(&JsValue::from_str(&format!("session: {what}")));
}
fn check(ok: bool, what: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(Error::message(what)) }
}

async fn roundtrip() -> Result<String> {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&JsValue::from_str(&format!("panic: {info}")));
    }));
    let factory = WebIdbFactory::global()?;
    let name = "session-roundtrip";
    factory.delete(name).await?;
    step("deleted");
    let first = session(Arc::new(IndexedDbStorage::open_with(&factory, name).await?));
    step("opened");
    // Concurrent commits serialize on the Session line.
    step("start bumps");
    let (a, b, c) = futures::join!(bump(&first), bump(&first), bump(&first));
    a?;
    b?;
    c?;
    step("bumped x3");
    check(count(&first).await? == 3, "three concurrent commits")?;
    step("counted");
    let conversation = first
        .commit(|tx| async move { Ok(tx.create_conversation(ConversationOwnership::Ownerless).await?.id) }, &context())
        .await?;
    first.close(&context()).await?;
    step("closed first");
    let reopened_storage = Arc::new(IndexedDbStorage::open_with(&factory, name).await?);
    let second = session(reopened_storage.clone());
    check(count(&second).await? == 3, "count survives reopen")?;
    step("reopened count");
    bump(&second).await?;
    check(count(&second).await? == 4, "commit after reopen")?;
    check(reopened_storage.conversation(conversation, &context()).await?.is_some(), "conversation survives reopen")?;
    second.close(&context()).await?;
    Ok(format!("{{\"conversation\":{}}}", conversation.get()))
}

#[wasm_bindgen]
pub async fn session_roundtrip() -> std::result::Result<String, JsValue> {
    roundtrip().await.map_err(|e| JsValue::from_str(&e.to_string()))
}
