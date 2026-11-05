//! `AlarmHost` and `KvHost` over the JavaScript `ctx.storage` of a Durable Object.

use super::{AlarmHost, KvHost};
use crate::{Error, JsonValue, storage::StorageFuture};
use js_sys::{Array, Function, Promise, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

struct Local<T>(T);
// SAFETY: Workers run one thread; see `storage::sqlite::cloudflare_js`.
unsafe impl<T> Send for Local<T> {}
unsafe impl<T> Sync for Local<T> {}

fn failure(value: JsValue) -> Error {
    Error::message(
        Reflect::get(&value, &JsValue::from_str("message"))
            .ok()
            .and_then(|message| message.as_string())
            .or_else(|| value.as_string())
            .unwrap_or_else(|| "Durable Object storage failed".into()),
    )
}
fn call(target: &JsValue, method: &str, args: &[JsValue]) -> Result<JsValue, Error> {
    let function = Reflect::get(target, &JsValue::from_str(method))
        .map_err(failure)?
        .dyn_into::<Function>()
        .map_err(|_| Error::message(format!("{method} is not a function")))?;
    function
        .apply(target, &args.iter().collect::<Array>())
        .map_err(failure)
}
/// Await a JavaScript promise on the Worker's thread behind a `Send` future.
fn settle<T: 'static>(
    start: impl FnOnce() -> Result<Promise, Error> + 'static,
    finish: impl FnOnce(JsValue) -> T + 'static,
) -> StorageFuture<T> {
    struct Wrapped<F>(Local<F>);
    impl<F: std::future::Future> std::future::Future for Wrapped<F> {
        type Output = F::Output;
        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            unsafe { self.map_unchecked_mut(|wrapped| &mut (wrapped.0).0) }.poll(cx)
        }
    }
    Box::pin(Wrapped(Local(async move {
        let value = JsFuture::from(start()?).await.map_err(failure)?;
        Ok(finish(value))
    })))
}

/// `ctx.storage` of a SQLite-backed Durable Object.
#[derive(Clone)]
pub struct JsStorage(std::sync::Arc<Local<JsValue>>);
impl JsStorage {
    pub fn new(storage: JsValue) -> Self {
        Self(std::sync::Arc::new(Local(storage)))
    }
    fn promise(
        &self,
        method: &'static str,
        args: Vec<JsValue>,
    ) -> impl FnOnce() -> Result<Promise, Error> + 'static {
        let storage = self.0.clone();
        move || {
            call(&storage.0, method, &args)?
                .dyn_into::<Promise>()
                .map_err(|_| Error::message(format!("{method} did not return a promise")))
        }
    }
}
impl AlarmHost for JsStorage {
    fn get_alarm(&self) -> StorageFuture<Option<f64>> {
        settle(self.promise("getAlarm", vec![]), |value| value.as_f64())
    }
    fn set_alarm(&self, at: f64) -> StorageFuture<()> {
        settle(
            self.promise("setAlarm", vec![JsValue::from_f64(at)]),
            |_| (),
        )
    }
    fn delete_alarm(&self) -> StorageFuture<()> {
        settle(self.promise("deleteAlarm", vec![]), |_| ())
    }
}
impl KvHost for JsStorage {
    fn get(&self, key: &str) -> Option<JsonValue> {
        let kv = Reflect::get(&self.0.0, &JsValue::from_str("kv")).ok()?;
        let value = call(&kv, "get", &[JsValue::from_str(key)]).ok()?;
        if value.is_undefined() {
            return None;
        }
        let text = js_sys::JSON::stringify(&value).ok()?.as_string()?;
        pi_chord::json::parse(&text).ok()
    }
    fn put(&self, key: &str, value: JsonValue) {
        let Ok(kv) = Reflect::get(&self.0.0, &JsValue::from_str("kv")) else {
            return;
        };
        if let Ok(parsed) = js_sys::JSON::parse(&value.to_string()) {
            let _ = call(&kv, "put", &[JsValue::from_str(key), parsed]);
        }
    }
}
