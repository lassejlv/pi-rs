//! A Harness hosted by a Durable Object and woken by its alarm.

use super::{AlarmHost, KvHost, arm, arm_fallback, workers_timer};
use crate::{Context, Harness, HarnessOptions, Result, Storage, harness::scheduler::Wake};
use parking_lot::Mutex;
use std::{future::Future, sync::Arc};

/// What a Durable Object gives its Harness besides SQL storage.
#[derive(Clone)]
pub struct DurableObjectHost {
    pub alarms: Arc<dyn AlarmHost>,
    pub kv: Arc<dyn KvHost>,
    /// Reset the object (`ctx.abort`): its instance is dropped and the next event starts a new one.
    pub abort: Arc<dyn Fn(&str) + Send + Sync>,
    /// Wall-clock milliseconds.
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
}

#[cfg(target_arch = "wasm32")]
fn spawn(future: impl Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(future);
}
#[cfg(not(target_arch = "wasm32"))]
fn spawn(future: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(future);
}

struct Inner {
    host: DurableObjectHost,
    offset: Mutex<f64>,
    wakes: Mutex<Vec<Wake>>,
}
impl Inner {
    fn offset(&self) -> f64 {
        *self.offset.lock()
    }
    fn clock(&self) -> f64 {
        (self.host.now)() + self.offset()
    }
    async fn arm(&self, wake: Wake) -> Result<()> {
        arm(
            self.host.alarms.as_ref(),
            &wake,
            (self.host.now)(),
            self.offset(),
        )
        .await
    }
}

/// The object-side half of `worker.ts`: opens a Harness whose wakes arm the
/// object's alarm, and resumes it when the alarm fires.
#[derive(Clone)]
pub struct WakeSession {
    inner: Arc<Inner>,
    harness: Harness,
}
impl WakeSession {
    /// Run in the object's constructor, inside `blockConcurrencyWhile`.
    pub async fn open(
        storage: Arc<dyn Storage>,
        mut options: HarnessOptions,
        host: DurableObjectHost,
        context: &Context,
    ) -> Result<Self> {
        arm_fallback(host.alarms.as_ref(), (host.now)()).await?;
        let offset = host
            .kv
            .get("offset")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0);
        let inner = Arc::new(Inner {
            host,
            offset: Mutex::new(offset),
            wakes: Mutex::default(),
        });
        let clock = inner.clone();
        options.now = Some(Arc::new(move || Ok(clock.clock())));
        options.set_timer.get_or_insert_with(workers_timer);
        let woken = inner.clone();
        options.on_wake = Some(Arc::new(move |wake| {
            woken.wakes.lock().push(wake);
            let inner = woken.clone();
            // A failed alarm write resets the object; it arms again when it next starts.
            spawn(async move {
                if inner.arm(wake).await.is_err() {
                    (inner.host.abort)("Alarm write failed");
                }
            });
            Ok(())
        }));
        let harness = Harness::open(storage, options, context).await?;
        // Armed before `resume()` runs anything, as for any call that can start work.
        inner.arm(harness.wake()?).await?;
        harness.resume()?;
        Ok(Self { inner, harness })
    }
    pub fn harness(&self) -> &Harness {
        &self.harness
    }
    /// The Harness clock: wall-clock time plus the test offset.
    pub fn now(&self) -> f64 {
        self.inner.clock()
    }
    /// The wakes `on_wake` received, in order.
    pub fn wakes(&self) -> Vec<Wake> {
        self.inner.wakes.lock().clone()
    }
    /// Move the Harness clock forward, for this instance and the ones after it.
    pub fn advance(&self, ms: f64) {
        let mut offset = self.inner.offset.lock();
        *offset += ms;
        self.inner
            .host
            .kv
            .put("offset", crate::wire::encode(&*offset));
    }
    /// Run when the object's alarm fires.
    pub async fn alarm(&self) -> Result<()> {
        let wake = self.harness.wake()?;
        self.inner
            .arm(Wake {
                active: true,
                ..wake
            })
            .await?;
        self.harness.resume()
    }
    /// Arm the alarm for `wake`.
    pub async fn arm(&self, wake: Wake) -> Result<()> {
        self.inner.arm(wake).await
    }
    /// Arm before admitting work: a process that dies after the commit is still woken.
    pub async fn arm_active(&self) -> Result<()> {
        let wake = self.harness.wake()?;
        self.inner
            .arm(Wake {
                active: true,
                ..wake
            })
            .await
    }
}
