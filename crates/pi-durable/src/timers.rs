//! Host timers with guaranteed foreground progress and optional background work.
use crate::Error;
use parking_lot::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
pub type TimerCallback = Box<dyn FnOnce() + Send>;
pub type DisarmTimer = Box<dyn FnOnce() + Send>;
pub type SetTimer = Arc<dyn Fn(TimerCallback, f64, bool) -> Option<DisarmTimer> + Send + Sync>;
type Report = Arc<dyn Fn(Error) + Send + Sync>;
const MAX_TIMER_DELAY: f64 = 2_147_483_647.0;

/// Tokio tasks do not keep the process/runtime alive after its owner shuts down.
/// Long delays are waited in bounded steps, as in the reference native timer.
pub fn native_timer(callback: TimerCallback, delay_ms: f64, _background: bool) -> DisarmTimer {
    let (cancel, receive) = tokio::sync::oneshot::channel();
    let cancel = Arc::new(Mutex::new(Some(cancel)));
    let keepalive = cancel.clone();
    pi_chord::rt::spawn(async move {
        let _keepalive = keepalive;
        let wait = async move {
            let mut remaining = if delay_ms.is_nan() {
                0.0
            } else {
                delay_ms.max(0.0)
            };
            loop {
                pi_chord::rt::sleep(std::time::Duration::from_secs_f64(
                    remaining.min(MAX_TIMER_DELAY) / 1000.0,
                ))
                .await;
                if remaining <= MAX_TIMER_DELAY {
                    callback();
                    break;
                }
                remaining -= MAX_TIMER_DELAY;
            }
        };
        tokio::select! {_=wait=>{},_=receive=>{}}
    });
    Box::new(move || {
        if let Some(cancel) = cancel.lock().take() {
            let _ = cancel.send(());
        }
    })
}
struct Inner {
    host: Option<SetTimer>,
    report: Report,
    foreground_reported: AtomicBool,
}
#[derive(Clone)]
pub struct Timers {
    inner: Arc<Inner>,
}
impl Timers {
    pub fn new(host: Option<SetTimer>, report: Report) -> Self {
        Self {
            inner: Arc::new(Inner {
                host,
                report,
                foreground_reported: AtomicBool::new(false),
            }),
        }
    }
    fn report(&self, error: Error) {
        let _ =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.inner.report)(error)));
    }
    fn fallback(&self, error: Error) {
        if !self.inner.foreground_reported.swap(true, Ordering::AcqRel) {
            self.report(error);
        }
    }
    fn arm(&self, callback: TimerCallback, delay_ms: f64, background: bool) -> Option<DisarmTimer> {
        let Some(host) = &self.inner.host else {
            return Some(native_timer(callback, delay_ms, background));
        };
        // A declined/throwing host consumed its callback argument. A single shared
        // slot lets the native fallback retain that same operation without a clone.
        let slot = Arc::new(Mutex::new(Some(callback)));
        let forwarded = slot.clone();
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            host(
                Box::new(move || {
                    let callback = forwarded.lock().take();
                    if let Some(callback) = callback {
                        callback();
                    }
                }),
                delay_ms,
                background,
            )
        }));
        let disarm = match attempt {
            Ok(Some(disarm)) => disarm,
            Ok(None) if background => return None,
            Err(panic) if background => {
                self.report(crate::session::panic_error(panic));
                return None;
            }
            fallback => {
                let error = match fallback {
                    Ok(None) => Error::message(
                        "HarnessOptions.setTimer declined a timer that paces work; using setTimeout",
                    ),
                    Err(p) => crate::session::panic_error(p),
                    _ => unreachable!(),
                };
                self.fallback(error);
                let forwarded = slot.clone();
                native_timer(
                    Box::new(move || {
                        let callback = forwarded.lock().take();
                        if let Some(callback) = callback {
                            callback();
                        }
                    }),
                    delay_ms,
                    false,
                )
            }
        };
        Some(Box::new(move || {
            slot.lock().take();
            disarm();
        }))
    }
    pub fn foreground(&self, callback: TimerCallback, delay_ms: f64) -> DisarmTimer {
        self.arm(callback, delay_ms, false)
            .expect("foreground always has a native timer")
    }
    pub fn background(&self, callback: TimerCallback, delay_ms: f64) -> Option<DisarmTimer> {
        self.arm(callback, delay_ms, true)
    }
}
