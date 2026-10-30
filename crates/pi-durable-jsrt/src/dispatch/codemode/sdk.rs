//! `CodemodeSandbox.start` of `@earendil-works/pi-codemode` (`js/pi-codemode/index.js`) over
//! `pi_codemode::CodemodeSandbox::start_json_utf16`: the VM is QuickJS on its own thread; its
//! `onStarted`/`onCall`/`onIdle` callbacks reach JS in VM order through one queue per run, and
//! the run's result settles only after every callback queued before it was delivered.

use std::sync::Arc;

use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use parking_lot::Mutex;
use pi_codemode::{
    AbortController, CodemodeCallTarget, CodemodeDeclaredGlobal, CodemodeJsonDeclaredTool,
    CodemodeJsonOutcome, CodemodeJsonRun, CodemodeJsonRunHandle, CodemodeJsonRunOptions,
    CodemodeSandbox,
};
use pi_json::strict::{JsonString, Map, Value};
use pi_jsrt::JsHandle;

use crate::errors::{ErrorClass, HostError, HostResult};
use crate::host::{Args, Call, Methods, call_async};
use crate::value::HostValue;

/// A sandbox's native state: the native sandbox and the glue's tool adapter.
pub struct Sandbox(pub CodemodeSandbox, Option<super::live::Callbacks>);

impl Sandbox {
    pub(super) fn live(sandbox: CodemodeSandbox, callbacks: super::live::Callbacks) -> Self {
        Sandbox(sandbox, Some(callbacks))
    }

    pub(super) fn callbacks(&self) -> Option<&super::live::Callbacks> {
        self.1.as_ref()
    }
}

/// One started run.
pub struct Run {
    handle: CodemodeJsonRunHandle,
    run: Mutex<Option<CodemodeJsonRun>>,
    controller: AbortController,
    events: mpsc::UnboundedSender<Event>,
}

enum Event {
    Started(String),
    Call(pi_codemode::CodemodeJsonHostCall),
    Idle(u64, u64),
    Flush(oneshot::Sender<()>),
}

fn number(value: &HostValue, name: &str) -> HostResult<Option<f64>> {
    match value {
        HostValue::Undefined => Ok(None),
        HostValue::Number(n) => Ok(Some(*n)),
        _ => Err(HostError::type_error(format!("{name} must be a number"))),
    }
}

fn field<'a>(value: &'a HostValue, key: &str) -> &'a HostValue {
    value.get(key).unwrap_or(&HostValue::Undefined)
}

fn text(value: &HostValue, name: &str) -> HostResult<JsonString> {
    value
        .as_string()
        .cloned()
        .ok_or_else(|| HostError::type_error(format!("{name} must be a string")))
}

fn optional_text(value: &HostValue, name: &str) -> HostResult<Option<JsonString>> {
    if value.is_undefined() {
        return Ok(None);
    }
    text(value, name).map(Some)
}

fn items(value: &HostValue) -> &[HostValue] {
    match value {
        HostValue::Array(items) => items,
        _ => &[],
    }
}

fn host_call(call: pi_codemode::CodemodeJsonHostCall) -> HostValue {
    let mut fields = vec![
        ("id", HostValue::from(call.id as f64)),
        (
            "target",
            HostValue::string(if call.target == CodemodeCallTarget::Tool {
                "tool"
            } else {
                "global"
            }),
        ),
        ("name", HostValue::String(call.name)),
    ];
    // `args` is always a member; a call without arguments passes `undefined`.
    fields.push((
        "args",
        call.args
            .as_ref()
            .map_or(HostValue::Undefined, HostValue::from_strict),
    ));
    HostValue::object(fields)
}

/// Deliver queued VM callbacks to JS, one at a time and in order.
async fn drain(mut rx: mpsc::UnboundedReceiver<Event>, target: JsHandle) {
    while let Some(event) = rx.next().await {
        let (kind, payload) = match event {
            Event::Flush(done) => {
                let _ = done.send(());
                continue;
            }
            Event::Started(fingerprint) => ("started", HostValue::string(&fingerprint)),
            Event::Call(call) => ("call", host_call(call)),
            Event::Idle(seq, issued) => (
                "idle",
                HostValue::Array(vec![
                    HostValue::from(seq as f64),
                    HostValue::from(issued as f64),
                ]),
            ),
        };
        // A throwing callback cannot fail the VM; the callback owner reports it.
        let _ = call_async(&target, None, vec![HostValue::string(kind), payload]).await;
    }
}

fn start(call: &Call, args: Args) -> HostResult<HostValue> {
    let sandbox = args.native::<Sandbox>(0)?;
    let code = args.string(1)?.clone();
    let options = args.get(2);
    let events = field(options, "events")
        .as_handle()
        .cloned()
        .ok_or_else(|| HostError::type_error("events must be a function"))?;
    let (tx, rx) = mpsc::unbounded();
    let started_at = number(field(options, "startedAt"), "startedAt")?.unwrap_or(f64::NAN);
    let seed = number(field(options, "seed"), "seed")?.unwrap_or(0.0);
    let calls = tx.clone();
    let mut native = CodemodeJsonRunOptions::new(started_at, seed as u32, move |host, _| {
        let _ = calls.unbounded_send(Event::Call(host));
    });
    let started = tx.clone();
    native.on_started = Some(Arc::new(move |fingerprint: &str| {
        let _ = started.unbounded_send(Event::Started(fingerprint.to_string()));
    }));
    let idle = tx.clone();
    native.on_idle = Some(Arc::new(move |seq, issued| {
        let _ = idle.unbounded_send(Event::Idle(seq, issued));
    }));
    native.tools = items(field(options, "tools"))
        .iter()
        .map(|tool| {
            Ok(CodemodeJsonDeclaredTool {
                name: text(field(tool, "name"), "tool name")?,
                js_name: optional_text(field(tool, "jsName"), "jsName")?,
                description: optional_text(field(tool, "description"), "description")?,
            })
        })
        .collect::<HostResult<_>>()?;
    native.globals = items(field(options, "globals"))
        .iter()
        .map(|global| {
            Ok(CodemodeDeclaredGlobal {
                name: text(field(global, "name"), "global name")?.to_string_lossy(),
                spread: field(global, "spread").as_bool().unwrap_or(false),
            })
        })
        .collect::<HostResult<_>>()?;
    native.store = match field(options, "store") {
        HostValue::Undefined => None,
        store => match store.to_strict_omitting_undefined()? {
            Value::Object(map) => Some(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
            _ => return Err(HostError::type_error("store must be an object")),
        },
    };
    native.timeout_ms = number(field(options, "timeoutMs"), "timeoutMs")?;
    let controller = AbortController::new();
    if field(options, "aborted").as_bool() == Some(true) {
        abort_controller(&controller, field(options, "reason"));
    }
    native.signal = Some(controller.signal());
    let run = sandbox
        .0
        .start_json_utf16(code.units(), native)
        .map_err(|e| HostError::new(ErrorClass::Error, e.to_string()))?;
    call.runtime().tokio().spawn(drain(rx, events));
    Ok(HostValue::native(Run {
        handle: run.handle(),
        run: Mutex::new(Some(run)),
        controller,
        events: tx,
    }))
}

fn abort_controller(controller: &AbortController, reason: &HostValue) {
    match reason.as_string() {
        Some(message) => controller.abort_with_reason(message.to_string_lossy()),
        None => controller.abort(),
    }
}

fn outcome(value: &HostValue) -> HostResult<CodemodeJsonOutcome> {
    if field(value, "ok").as_bool() == Some(true) {
        let value = match field(value, "value") {
            HostValue::Undefined => None,
            v => Some(v.to_strict_omitting_undefined()?),
        };
        return Ok(CodemodeJsonOutcome::Ok { value });
    }
    let message = match field(value, "message") {
        HostValue::String(s) => s.clone(),
        _ => JsonString::from(""),
    };
    let properties = match field(value, "properties") {
        HostValue::Undefined => None,
        v => match v.to_strict_omitting_undefined()? {
            Value::Object(map) => Some(
                map.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Map>(),
            ),
            _ => None,
        },
    };
    Ok(CodemodeJsonOutcome::Err {
        message,
        properties,
    })
}

pub(super) fn register(m: &mut Methods) {
    m.sync("codemode.sdk.sandboxNew", super::live::sandbox_new);
    super::live::register(m);
    m.asynchronous("codemode.sdk.sandboxClose", |_call, args| async move {
        let sandbox = args.native::<Sandbox>(0)?;
        sandbox.0.close().await;
        Ok(HostValue::Undefined)
    });
    m.sync("codemode.sdk.start", start);
    m.sync("codemode.sdk.finished", |_call, args| {
        Ok(HostValue::Bool(args.native::<Run>(0)?.handle.finished()))
    });
    m.sync("codemode.sdk.deliver", |_call, args| {
        let run = args.native::<Run>(0)?;
        let seq = args.f64(1)?;
        let now = args.f64(2)?;
        let id = args.f64(3)?;
        run.handle
            .deliver(seq as u64, now, id as u64, outcome(args.get(4))?)
            .map_err(|message| HostError::new(ErrorClass::Error, message))?;
        Ok(HostValue::Undefined)
    });
    m.sync("codemode.sdk.abort", |_call, args| {
        let run = args.native::<Run>(0)?;
        abort_controller(&run.controller, args.get(1));
        Ok(HostValue::Undefined)
    });
    m.asynchronous("codemode.sdk.result", |_call, args| async move {
        let run = args.native::<Run>(0)?;
        let pending = run
            .run
            .lock()
            .take()
            .ok_or_else(|| HostError::new(ErrorClass::Error, "run result already taken"))?;
        let result = pending.result().await;
        // Settle only after the callbacks the VM issued before its result reached JS.
        let (done, flushed) = oneshot::channel();
        if run.events.unbounded_send(Event::Flush(done)).is_ok() {
            let _ = flushed.await;
        }
        Ok(HostValue::from_strict(&result))
    });
}
