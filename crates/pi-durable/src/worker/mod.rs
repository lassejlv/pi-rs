//! Hosting a Harness in a Cloudflare Durable Object: the alarm that wakes the
//! scheduler and the timer policy of Workers. Host capabilities are traits, so the
//! same logic runs against Cloudflare's JavaScript storage (`js`, wasm32 with the
//! `cloudflare` feature) and against in-process hosts in tests.

pub use crate::harness::scheduler::Wake;
use crate::timers::{DisarmTimer, SetTimer, TimerCallback, native_timer};
use crate::{JsonValue, Result, storage::StorageFuture};
use std::sync::Arc;

#[cfg(all(target_arch = "wasm32", feature = "cloudflare"))]
pub mod js;
pub mod session;
pub use session::{DurableObjectHost, WakeSession};

/// Recovery heartbeat while work can run: an alarm this far ahead restarts an
/// object that dies mid-turn.
pub const HEARTBEAT_MS: f64 = 60_000.0;

/// Synchronous key-value storage of a SQLite-backed Durable Object (`ctx.storage.kv`).
pub trait KvHost: Send + Sync {
    fn get(&self, key: &str) -> Option<JsonValue>;
    fn put(&self, key: &str, value: JsonValue);
}

/// The alarm slot of a Durable Object (`ctx.storage.getAlarm/setAlarm/deleteAlarm`).
pub trait AlarmHost: Send + Sync {
    fn get_alarm(&self) -> StorageFuture<Option<f64>>;
    fn set_alarm(&self, at: f64) -> StorageFuture<()>;
    fn delete_alarm(&self) -> StorageFuture<()>;
}

/// The alarm a host arms for `wake`: the earliest sleep, or a recovery heartbeat
/// while work can run. `now` is wall-clock time; sleeps are in Harness time, which
/// runs `offset` ahead of it.
pub fn alarm_at(wake: &Wake, now: f64, offset: f64) -> Option<f64> {
    let heartbeat = wake.active.then_some(now + HEARTBEAT_MS);
    let sleep = wake.at.map(|at| at - offset);
    match (heartbeat, sleep) {
        (Some(heartbeat), Some(sleep)) => Some(heartbeat.min(sleep)),
        (heartbeat, sleep) => heartbeat.or(sleep),
    }
}

/// Set or delete the object's alarm for `wake`.
pub async fn arm(alarms: &dyn AlarmHost, wake: &Wake, now: f64, offset: f64) -> Result<()> {
    match alarm_at(wake, now, offset) {
        Some(at) => alarms.set_alarm(at).await,
        None => alarms.delete_alarm().await,
    }
}

/// A fallback armed before anything can fail: an object whose open or recovery
/// fails is started again by the alarm. An earlier alarm is kept.
pub async fn arm_fallback(alarms: &dyn AlarmHost, now: f64) -> Result<()> {
    let fallback = now + HEARTBEAT_MS;
    match alarms.get_alarm().await? {
        Some(current) if current <= fallback => Ok(()),
        _ => alarms.set_alarm(fallback).await,
    }
}

/// Timers for Workers: a timer that would only serve background work (retention
/// of idle documents and contexts) is declined, because a pending timer keeps an
/// object from being evicted. Foreground timers run.
pub fn workers_timer() -> SetTimer {
    Arc::new(
        |callback: TimerCallback, delay_ms: f64, background: bool| -> Option<DisarmTimer> {
            (!background).then(|| native_timer(callback, delay_ms, false))
        },
    )
}
