//! The stock durable service (`pi-durable/examples/task_stock_bridge.rs` and its
//! `examples/support/*` bridges) hosted in-process: one implementation serves both the
//! process-transport gate and the embedded packages.
//!
//! Frames are the stock wire format: one strict-JSON text per frame (`pi_json::strict`, lone
//! surrogates escaped), in both directions. JS (`js/internal/transport.js`, the in-process
//! `nativeTransport`) sends frames with `transport.send`; a reader thread routes them exactly as
//! the stock process routes stdin lines. Frames the service emits are queued: JS drains them
//! with `transport.take` when the runtime posts a pump, or with `transport.wait` while a
//! synchronous request blocks the JS thread (the stock transport's `Atomics.wait` loop).
//!
//! Methods (`t` is the transport returned by `transport.open`):
//! - `transport.open(pump)`: start a service; `pump()` is called on the JS thread whenever frames
//!   are queued and the JS thread is not waiting for them.
//! - `transport.send(t, frame)`: queue a frame for the service (never blocks).
//! - `transport.take(t)`: queued frames, oldest first.
//! - `transport.wait(t, timeoutMs)`: block until a frame is queued (or the timeout), then take.
//! - `transport.ackLater(t, count)`: acknowledge `count` delivered frames to the step gate once the
//!   JS thread's current task and its microtasks finish.
//! - `transport.memoryCall(t, request)`: run a `memory.call` request now, on the JS thread; returns
//!   its response frame (with `adopted`, the sequence a commit adopted).
//! - `transport.close(t)`: end the input; `{"type":"transport.exit","code":0}` follows once the
//!   service has drained.

#[allow(
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    clippy::all,
    clippy::pedantic
)]
#[path = "../../../pi-durable/examples/task_stock_bridge.rs"]
mod stock;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use pi_jsrt::{JsHandle, JsRuntime};

use crate::errors::HostError;
use crate::host::{Dispatch, Methods};
use crate::value::HostValue;

/// The transport method group.
pub struct TransportDispatch;

struct Queue {
    frames: Mutex<VecDeque<String>>,
    ready: Condvar,
    /// A pump is posted to the JS thread and has not run yet.
    pump_posted: AtomicBool,
    /// The runtime and pump, until the transport closes (they keep the runtime alive).
    pump: Mutex<Option<(JsRuntime, JsHandle)>>,
}

impl Queue {
    fn push(self: &Arc<Self>, frame: String) {
        self.frames.lock().push_back(frame);
        self.ready.notify_all();
        if self.pump_posted.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some((rt, pump)) = self.pump.lock().clone() else {
            return;
        };
        let queue = self.clone();
        // Posted directly: the emitting thread may be a tokio worker, and every worker may be
        // blocked on a synchronous callback that only this pump can deliver.
        let _ = rt.post_task(move |ctx| {
            queue.pump_posted.store(false, Ordering::Release);
            if queue.frames.lock().is_empty() {
                return;
            }
            if let Some(f) = pi_jsrt::ctx::handle_value(ctx, &pump).and_then(|f| f.as_callable())
                && let Err(e) = f.call(&pi_jsrt::js::JsValue::undefined(), &[], ctx)
            {
                let text = e
                    .into_opaque(ctx)
                    .map(|v| v.display().to_string())
                    .unwrap_or_default();
                eprintln!("pi-durable transport pump failed: {text}");
            }
        });
    }

    fn take(&self) -> HostValue {
        let frames: Vec<HostValue> = self
            .frames
            .lock()
            .drain(..)
            .map(|f| HostValue::string(&f))
            .collect();
        HostValue::Array(frames)
    }
}

/// One in-process service.
struct Transport {
    queue: Arc<Queue>,
    /// Frames for the reader thread; taken on close.
    input: Mutex<Option<std::sync::mpsc::Sender<String>>>,
    /// The admission lane; taken when the reader ends.
    lane: Mutex<Option<std::sync::mpsc::Sender<stock::Work>>>,
    hub: Arc<stock::Hub>,
    runtime: tokio::runtime::Handle,
    /// Frames sent to the reader and not yet routed.
    queued: AtomicUsize,
}

fn transport(args: &crate::host::Args) -> Result<Arc<Transport>, HostError> {
    args.native::<Transport>(0)
}

impl Dispatch for TransportDispatch {
    fn register(&self, m: &mut Methods) {
        m.sync("transport.open", |call, args| {
            let pump = args.handle(0)?;
            let queue = Arc::new(Queue {
                frames: Mutex::new(VecDeque::new()),
                ready: Condvar::new(),
                pump_posted: AtomicBool::new(false),
                pump: Mutex::new(Some((call.runtime().clone(), pump))),
            });
            let runtime = call.runtime().tokio().clone();
            let _entered = runtime.enter();
            let output = queue.clone();
            let hub = stock::new_hub(Box::new(move |line: &str| output.push(line.to_string())));
            let (lane, lane_thread) = stock::spawn_lane(&hub, &runtime);
            let (input, lines) = std::sync::mpsc::channel::<String>();
            let transport = Arc::new(Transport {
                queue,
                input: Mutex::new(Some(input)),
                lane: Mutex::new(Some(lane.clone())),
                hub,
                runtime: runtime.clone(),
                queued: AtomicUsize::new(0),
            });
            let reader = transport.clone();
            std::thread::Builder::new()
                .name("pi-durable-transport".into())
                .spawn(move || {
                    let _entered = reader.runtime.enter();
                    for line in lines {
                        match pi_json::strict::parse(&line) {
                            Ok(value) => stock::receive(&reader.hub, &reader.runtime, &lane, value),
                            Err(e) => {
                                reader.queue.push(format!(
                                    "{{\"type\":\"transport.error\",\"message\":{}}}",
                                    pi_json::strict::stringify(&pi_json::strict::Value::from(
                                        e.as_str()
                                    ))
                                ));
                                break;
                            }
                        }
                        reader.queued.fetch_sub(1, Ordering::AcqRel);
                    }
                    stock::close_input(&reader.hub);
                    drop(lane);
                    reader.lane.lock().take();
                    let _ = lane_thread.join();
                    reader
                        .queue
                        .push("{\"type\":\"transport.exit\",\"code\":0}".to_string());
                })
                .map_err(|e| HostError::new(crate::ErrorClass::Error, e.to_string()))?;
            Ok(HostValue::Native(crate::value::NativeRef(transport)))
        });
        m.sync("transport.send", |_, args| {
            let t = transport(&args)?;
            let frame = args.text(1)?;
            // A callback reply only completes a waiter (or queues it on the lane): with nothing
            // ahead of it on the reader, route it here and save the reader's wakeup.
            if frame.starts_with("{\"type\":\"callback.result\"")
                && t.queued.load(Ordering::Acquire) == 0
            {
                let lane = t.lane.lock().clone();
                if let (Some(lane), Ok(value)) = (lane, pi_json::strict::parse(&frame)) {
                    let _entered = t.runtime.enter();
                    stock::receive(&t.hub, &t.runtime, &lane, value);
                    return Ok(HostValue::Undefined);
                }
            }
            let input = t.input.lock();
            let Some(input) = input.as_ref() else {
                return Err(HostError::new(
                    crate::ErrorClass::Error,
                    "Native durable transport closed",
                ));
            };
            t.queued.fetch_add(1, Ordering::AcqRel);
            if input.send(frame).is_err() {
                t.queued.fetch_sub(1, Ordering::AcqRel);
                return Err(HostError::new(
                    crate::ErrorClass::Error,
                    "Native durable transport closed",
                ));
            }
            Ok(HostValue::Undefined)
        });
        // `memory.call` on the JS thread: one host call instead of a request frame, a reader and
        // lane hop and a response pump. Native memory operations complete when called, so the
        // result (a response-shaped frame with the adopted sequence) is ready at once.
        m.sync("transport.memoryCall", |_, args| {
            let t = transport(&args)?;
            let request = pi_json::strict::parse(&args.text(1)?)
                .map_err(|e| HostError::new(crate::ErrorClass::TypeError, e))?;
            let _entered = t.runtime.enter();
            let frame = t.hub.memory_call_now(&request);
            Ok(HostValue::string(&pi_json::strict::stringify(&frame)))
        });
        // Acknowledge `count` delivered frames once the JS thread finishes its current task and
        // microtasks: a posted task, so test timer fakes and timer counts never see it.
        m.sync("transport.ackLater", |call, args| {
            let t = transport(&args)?;
            let count = args.f64(1)? as u64;
            // Through the reader like every request, so requests sent before it are routed first.
            let _ = call.runtime().post_task(move |_| {
                if let Some(input) = t.input.lock().as_ref() {
                    t.queued.fetch_add(1, Ordering::AcqRel);
                    if input
                        .send(format!("{{\"type\":\"host.ack\",\"count\":{count}}}"))
                        .is_err()
                    {
                        t.queued.fetch_sub(1, Ordering::AcqRel);
                    }
                }
            });
            Ok(HostValue::Undefined)
        });
        m.sync("transport.take", |_, args| {
            Ok(transport(&args)?.queue.take())
        });
        m.sync("transport.wait", |_, args| {
            let t = transport(&args)?;
            let timeout = Duration::from_secs_f64(args.f64(1)?.max(0.0) / 1000.0);
            let mut frames = t.queue.frames.lock();
            if frames.is_empty() {
                t.queue.ready.wait_for(&mut frames, timeout);
            }
            drop(frames);
            Ok(t.queue.take())
        });
        m.sync("transport.close", |_, args| {
            let t = transport(&args)?;
            t.input.lock().take();
            Ok(HostValue::Undefined)
        });
        m.sync("transport.release", |_, args| {
            // Drop the runtime reference held for pumps (after the exit frame was seen).
            transport(&args)?.queue.pump.lock().take();
            Ok(HostValue::Undefined)
        });
    }
}
