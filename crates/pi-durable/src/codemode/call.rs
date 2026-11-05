//! One durable script invocation. VM callbacks enter one actor; storage commits
//! retire independently, so callback admission never holds a lock over I/O.
use super::{describe::*, models::*, output::*, types::*, vm::*};
use crate::{
    harness::{live::DiagnosticSeverity, tool::*},
    storage::{StorageFuture, line::eager},
    *,
};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use indexmap::IndexMap;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Arc,
};
use tokio::sync::mpsc;

pub const OUTPUT_BYTES: usize = 4 * 1024 * 1024;
pub const OUTPUT_LINES: usize = 100_000;
const DISCOVERY: &[&str] = &["searchTools", "describeTool", "describeNamespace"];
struct Found {
    start: Start,
    journal: JournalState,
    rerun: bool,
}
enum Prepared {
    Done(ToolExecutionResult),
    Run(Found),
}
#[derive(Clone)]
struct Delivery {
    call: HostCall,
    at: f64,
    outcome: Outcome,
    global: Option<GlobalEntry>,
}
struct ToolDone {
    outcome: Outcome,
    result: NestedToolExecutionResult,
}
enum Event {
    Started(JsonString),
    Call(HostCall),
    Idle(u64, u64),
    VmDone(Result<JsonValue>),
    Fingerprint(Result<()>),
    Released(Vec<HostCall>, Result<Option<(HostCall, JsonString)>>),
    ToolDone(HostCall, bool, Box<Result<ToolDone>>),
    GlobalDone(HostCall, Result<Outcome>),
    Pumped(Vec<Delivery>, Vec<(u64, u64)>, Result<()>),
    Decided(Result<()>),
}
enum Release {
    Held,
    Checking,
    Live,
}
struct Call {
    options: Arc<CodemodeToolOptions>,
    api: ToolExecutionApi,
    context: Context,
    start: Start,
    run: Arc<dyn Run>,
    controller: pi_chord::AbortController,
    replayed: Vec<JournalEntry>,
    replay_index: HashMap<u64, usize>,
    first_issued: Option<u64>,
    rerun: bool,
    replaying: bool,
    next_replayed: usize,
    journal_length: usize,
    durable_issued: u64,
    issued: u64,
    gate: bool,
    release: Release,
    held: Vec<HostCall>,
    waiting: Vec<HostCall>,
    ready: HashMap<u64, Outcome>,
    queue: Vec<Delivery>,
    acks: IndexMap<u64, u64>,
    committing: bool,
    model_calls: usize,
    model_waiters: VecDeque<(HostCall, bool)>,
    generated_images: usize,
    rows: BTreeMap<u64, CodemodeNestedCall>,
    failure: Option<Error>,
    ended: Option<ToolExecutionResult>,
    caller_aborted: bool,
    receiver: mpsc::UnboundedReceiver<Event>,
    jobs: FuturesUnordered<BoxFuture<'static, Event>>,
}
impl Drop for Call {
    fn drop(&mut self) {
        self.controller.abort();
    }
}
fn error_result(text: impl Into<JsonString>) -> ToolExecutionResult {
    ToolExecutionResult {
        output: Some(Arc::new(vec![text_content(text.into())])),
        is_error: Some(true),
        ..Default::default()
    }
}
fn cut(text: JsonString, max: usize) -> JsonString {
    if text.units().len() > max {
        join(
            [
                JsonString::from_utf16(text.units()[..max.saturating_sub(3)].to_vec()),
                "...".into(),
            ],
            "",
        )
    } else {
        text
    }
}
fn arguments(call: &HostCall) -> JsonValue {
    call.args
        .clone()
        .filter(|value| !value.is_null())
        .unwrap_or_else(wire::object)
}
fn text_of(value: Option<&JsonValue>) -> JsonString {
    match value {
        Some(JsonValue::String(text)) => text.clone(),
        Some(JsonValue::Array(items)) => join(
            items
                .iter()
                .filter(|v| v["type"].string_equals("text"))
                .filter_map(|v| v["text"].as_string().cloned()),
            "\n",
        ),
        _ => JsonString::default(),
    }
}
fn tool_outcome(
    name: &JsonString,
    structured: bool,
    result: &NestedToolExecutionResult,
) -> Outcome {
    let value = if structured {
        result.structured_output.clone()
    } else {
        Some(JsonValue::String(text_of(
            result.structured_output.as_ref(),
        )))
    };
    if !result.is_error || structured && value.is_some() {
        return Outcome::Success(value);
    }
    let message = if structured {
        JsonString::default()
    } else {
        text_of(value.as_ref())
    };
    let diagnostics = join(
        result
            .diagnostics
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .map(|d| d.message.clone()),
        "\n",
    );
    let message = if !message.units().is_empty() {
        message
    } else if !diagnostics.units().is_empty() {
        diagnostics
    } else {
        join(["Tool ".into(), name.clone(), " failed".into()], "")
    };
    Outcome::Failure {
        message,
        properties: value.map(|value| {
            let mut object = JsonObject::default();
            object.insert("structuredOutput".into(), value);
            object
        }),
    }
}
fn mapped_call(mut call: HostCall) -> HostCall {
    let tool = if call.target == CallTarget::Global {
        match call.name.to_string_checked().ok().as_deref() {
            Some("models.classify") => Some(CLASSIFY_TOOL),
            Some("models.generateImages") => Some(GENERATE_IMAGES_TOOL),
            _ => None,
        }
    } else {
        None
    };
    if let Some(tool) = tool {
        let args = call.args.as_ref().and_then(JsonValue::as_array);
        let mut input = JsonObject::default();
        input.insert(
            "model".into(),
            args.and_then(|a| a.first())
                .cloned()
                .unwrap_or(JsonValue::Null),
        );
        input.insert(
            "context".into(),
            args.and_then(|a| a.get(1))
                .cloned()
                .unwrap_or(JsonValue::Null),
        );
        call.target = CallTarget::Tool;
        call.name = tool.into();
        call.args = Some(input.into_value());
    }
    call
}
fn details(rows: &BTreeMap<u64, CodemodeNestedCall>) -> CodemodeToolDetails {
    CodemodeToolDetails {
        calls: rows
            .values()
            .map(|row| {
                let mut row = row.clone();
                if row.status.units() == JsonString::from("running").units() {
                    row.status = "cancelled".into();
                }
                row
            })
            .collect(),
        full_output_path: None,
    }
}
fn decide(
    api: &ToolExecutionApi,
    context: &Context,
    result: ToolExecutionResult,
    details: CodemodeToolDetails,
) -> StorageFuture<ToolExecutionResult> {
    let mut result = result;
    result.details = Some(wire::encode(&details).into());
    let saved = result.clone();
    let task = api.task_id().untyped();
    let pending = api.commit(
        move |tx| async move {
            tx.doc(&CODEMODE_RUN_DOC, task).await?.update(|run| {
                if run.result.is_none() {
                    run.result = Some(
                        JsonObject::try_from_value(wire::encode(&saved)).map_err(Error::message)?,
                    );
                }
                Ok(())
            })
        },
        context,
    );
    Box::pin(async move {
        pending.await?;
        Ok(result)
    })
}
pub fn execute(
    options: Arc<CodemodeToolOptions>,
    api: ToolExecutionApi,
    context: Context,
    source: JsonString,
) -> StorageFuture<ToolExecutionResult> {
    eager(async move {
        let parsed = pi_codemode::parse_codemode_source_utf16(source.units())
            .map_err(|error| Error::host(error.name(), error.message))?;
        let agent = api.agent(&context).await?;
        let tools = agent
            .callable_tools()?
            .iter()
            .map(|tool| {
                let meta = ToolMetadata::of(tool.as_ref());
                let name = meta.declaration.name.clone();
                Ok(DeclaredTool {
                    js_name: identifier(&name),
                    description: sample_of(&meta)?,
                    structured: meta.structured_output_schema.is_some(),
                    hidden: name
                        .units()
                        .starts_with(&CODEMODE_INTERNAL_PREFIX.encode_utf16().collect::<Vec<_>>()),
                    name,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut globals: Vec<_> = DISCOVERY
            .iter()
            .map(|name| DeclaredGlobal {
                name: (*name).into(),
                spread: true,
            })
            .collect();
        if options.models {
            globals.extend(
                CATALOG_GLOBALS
                    .iter()
                    .chain(["models.classify", "models.generateImages"].iter())
                    .map(|name| DeclaredGlobal {
                        name: (*name).into(),
                        spread: true,
                    }),
            );
        }
        globals.extend(options.globals.iter().map(|global| DeclaredGlobal {
            name: global.name.clone(),
            spread: global.spread,
        }));
        let mut namespaces: Vec<StartNamespace> = vec![];
        for tool in tools.iter().filter(|tool| !tool.hidden) {
            if let Some(namespace) = namespace_of(&tool.name, &options.namespaces) {
                let index =
                    if let Some(index) = namespaces.iter().position(|n| n.name == namespace.name) {
                        index
                    } else {
                        namespaces.push(StartNamespace {
                            name: namespace.name.clone(),
                            description: namespace.description.clone(),
                            instructions: namespace.instructions.clone(),
                            tools: vec![],
                        });
                        namespaces.len() - 1
                    };
                namespaces[index].tools.push(tool.name.clone());
            }
        }
        let now = api.now()?;
        let seed = u32::from_ne_bytes(uuid::Uuid::new_v4().as_bytes()[..4].try_into().unwrap());
        let task = api.task_id().untyped();
        let conversation = api.conversation_id();
        let candidate = Start {
            version: CODEMODE_MODULE_VERSION,
            started_at: now,
            seed,
            fingerprint: None,
            deadline: parsed.options.timeout_ms.map(|ms| now + ms as f64),
            tools,
            namespaces,
            globals,
            max_output_tokens: parsed
                .options
                .max_output_tokens
                .map(|n| n as f64)
                .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
            store: JsonObject::default(),
        };
        let prepared = api
            .commit(
                move |tx| async move {
                    let run = tx.doc(&CODEMODE_RUN_DOC, task).await?;
                    let state = run.read()?;
                    if let Some(result) = state.result {
                        return Ok(Prepared::Done(
                            wire::decode(result.value()).map_err(Error::message)?,
                        ));
                    }
                    let rerun = state.start.is_some();
                    if !rerun {
                        let store = tx.doc(&CODEMODE_STORE_DOC, conversation).await?.read()?;
                        let mut candidate = candidate;
                        candidate.store = store.values;
                        run.update(|run| {
                            run.start = Some(candidate);
                            Ok(())
                        })?;
                    }
                    let start = run.read()?.start.expect("start installed");
                    let journal = tx.doc(&CODEMODE_JOURNAL_DOC, task).await?.read()?;
                    Ok(Prepared::Run(Found {
                        start,
                        journal,
                        rerun,
                    }))
                },
                &context,
            )
            .await?;
        let Prepared::Run(found) = prepared else {
            let Prepared::Done(result) = prepared else {
                unreachable!()
            };
            return Ok(result);
        };
        if found.start.version != CODEMODE_MODULE_VERSION {
            return decide(&api,&context,error_result(format!("The script cannot resume: it started under code mode version {}, this is {CODEMODE_MODULE_VERSION}.",found.start.version)),details(&BTreeMap::new())).await;
        }
        let remaining = found
            .start
            .deadline
            .map(|deadline| api.now().map(|now| deadline - now))
            .transpose()?;
        if remaining.is_some_and(|remaining| remaining <= 0.0) {
            return decide(
                &api,
                &context,
                error_result("Script timed out: its deadline passed while it was interrupted."),
                details(&BTreeMap::new()),
            )
            .await;
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        let started = sender.clone();
        let called = sender.clone();
        let idle = sender.clone();
        let controller = pi_chord::AbortController::new();
        let run = options.sandbox.start(
            parsed.code,
            RunOptions {
                start: found.start.clone(),
                timeout_ms: remaining,
                signal: controller.signal(),
                on_started: Arc::new(move |fp| {
                    let _ = started.send(Event::Started(fp));
                }),
                on_call: Arc::new(move |call| {
                    let _ = called.send(Event::Call(call));
                }),
                on_idle: Arc::new(move |seq, issued| {
                    let _ = idle.send(Event::Idle(seq, issued));
                }),
            },
        )?;
        let replay_index = found
            .journal
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id, index))
            .collect();
        let durable_issued = found
            .journal
            .entries
            .iter()
            .filter_map(|entry| entry.issued_after)
            .chain(found.journal.first_issued)
            .max()
            .unwrap_or(0);
        let jobs = FuturesUnordered::new();
        let result = run.result();
        jobs.push(async move { Event::VmDone(result.await) }.boxed());
        let mut call = Call {
            options,
            api,
            context,
            start: found.start,
            run,
            controller,
            replay_index,
            first_issued: found.journal.first_issued,
            rerun: found.rerun,
            replaying: !found.journal.entries.is_empty(),
            next_replayed: 0,
            journal_length: found.journal.entries.len(),
            replayed: found.journal.entries,
            durable_issued,
            issued: 0,
            gate: false,
            release: Release::Held,
            held: vec![],
            waiting: vec![],
            ready: HashMap::new(),
            queue: vec![],
            acks: IndexMap::new(),
            committing: false,
            model_calls: 0,
            model_waiters: VecDeque::new(),
            generated_images: 0,
            rows: BTreeMap::new(),
            failure: None,
            ended: None,
            caller_aborted: false,
            receiver,
            jobs,
        };
        call.drive().await
    })
}
impl Call {
    fn stopped(&self) -> bool {
        self.ended.is_some() || self.failure.is_some() || self.run.finished() || self.caller_aborted
    }
    fn fail(&mut self, error: Error) {
        if self.failure.is_none() {
            self.controller
                .abort_with_reason(pi_chord::context::AbortReason::Error(
                    pi_chord::context::AbortError::new(error.to_string()),
                ));
            self.failure = Some(error);
        }
    }
    fn end(&mut self, result: ToolExecutionResult) {
        if self.ended.is_some() || self.failure.is_some() {
            return;
        }
        self.ended = Some(result.clone());
        let pending = decide(&self.api, &self.context, result, details(&self.rows));
        self.jobs
            .push(async move { Event::Decided(pending.await.map(|_| ())) }.boxed());
    }
    fn diverge(&mut self, message: JsonString) {
        self.end(error_result(join(
            [
                "The script took a different path when resumed after a restart: ".into(),
                message,
            ],
            "",
        )));
    }
    async fn drive(&mut self) -> Result<ToolExecutionResult> {
        let signal = self.context.abort_signal().cloned();
        let cancelled = async move {
            if let Some(signal) = signal {
                signal.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::pin!(cancelled);
        loop {
            let event = tokio::select! {biased;
                _=&mut cancelled,if !self.caller_aborted=>{self.caller_aborted=true;let reason=self.context.abort_signal().and_then(|s|s.reason()).unwrap_or_else(||pi_chord::context::AbortReason::Error(pi_chord::context::AbortError::new("Operation aborted")));self.controller.abort_with_reason(reason);continue;},
                Some(event)=self.receiver.recv()=>event,
                Some(event)=self.jobs.next()=>event,
                else=>return Err(Error::message("Code-mode driver ended without a VM result")),
            };
            match event {
                Event::VmDone(result) => return self.finish(result?).await,
                Event::Started(fingerprint) => self.fingerprint(fingerprint),
                Event::Call(call) => self.on_call(mapped_call(call)),
                Event::Idle(seq, issued) => self.on_idle(seq, issued),
                Event::Fingerprint(result) => match result {
                    Err(error) => self.fail(error),
                    Ok(()) => self.open_gate()?,
                },
                Event::Decided(result) => match result {
                    Ok(()) => self.controller.abort(),
                    Err(error) => self.fail(error),
                },
                Event::Released(calls, result) => match result {
                    Err(error) => self.fail(error),
                    Ok(Some((call, other))) => self.diverge(join(
                        [
                            "call ".into(),
                            call.id.to_string().into(),
                            " is ".into(),
                            describe_call(&call),
                            ", but was ".into(),
                            other,
                        ],
                        "",
                    )),
                    Ok(None) => {
                        if !self.stopped() {
                            self.release = Release::Live;
                            self.waiting.extend(calls);
                            self.waiting.append(&mut self.held);
                            self.launch_covered();
                        }
                    }
                },
                Event::ToolDone(call, replay, result) => {
                    if call
                        .name
                        .units()
                        .starts_with(&CODEMODE_INTERNAL_PREFIX.encode_utf16().collect::<Vec<_>>())
                    {
                        self.model_calls -= 1;
                        self.next_model();
                    }
                    match *result {
                        Ok(done) => {
                            self.finish_row(&call, &done.outcome, &done.result);
                            if !self.stopped() {
                                if replay {
                                    self.ready.insert(call.id, done.outcome);
                                    self.deliver_replayed()?;
                                } else {
                                    self.queue.push(Delivery {
                                        at: self.api.now()?,
                                        call,
                                        outcome: done.outcome,
                                        global: None,
                                    });
                                    self.pump();
                                }
                            }
                        }
                        Err(error) => self.failed_call(call, error),
                    }
                }
                Event::GlobalDone(call, result) => match result {
                    Ok(outcome) => {
                        if !self.stopped() {
                            let global = GlobalEntry {
                                name: call.name.clone(),
                                args: call.args.clone(),
                                outcome: JsonObject::try_from_value(outcome.to_json())
                                    .map_err(Error::message)?,
                            };
                            self.queue.push(Delivery {
                                at: self.api.now()?,
                                call,
                                outcome,
                                global: Some(global),
                            });
                            self.pump();
                        }
                    }
                    Err(error) => self.failed_call(call, error),
                },
                Event::Pumped(batch, acks, result) => match result {
                    Err(error) => self.fail(error),
                    Ok(()) => {
                        for (_, issued) in acks {
                            self.durable_issued = self.durable_issued.max(issued);
                        }
                        for item in batch {
                            self.journal_length += 1;
                            if !self.run.finished() {
                                self.run.deliver(
                                    self.journal_length as u64,
                                    item.at,
                                    item.call.id,
                                    item.outcome,
                                )?;
                            }
                        }
                        self.launch_covered();
                        self.committing = false;
                        self.pump();
                    }
                },
            }
        }
    }
    fn open_gate(&mut self) -> Result<()> {
        self.gate = true;
        for call in self.waiting_replay() {
            self.start_tool(call, true);
        }
        if self.replaying {
            self.deliver_replayed()?;
        } else if !self.rerun {
            self.release();
        }
        Ok(())
    }
    fn fingerprint(&mut self, fingerprint: JsonString) {
        if let Some(recorded) = &self.start.fingerprint {
            if *recorded != fingerprint {
                self.end(error_result(join(
                    [
                        "The script cannot resume: the sandbox changed since it started (".into(),
                        recorded.clone(),
                        " -> ".into(),
                        fingerprint,
                        ").".into(),
                    ],
                    "",
                )));
                return;
            }
            if let Err(error) = self.open_gate() {
                self.fail(error);
            }
            return;
        }
        let task = self.api.task_id().untyped();
        let pending = self.api.commit(
            move |tx| async move {
                tx.doc(&CODEMODE_RUN_DOC, task).await?.update(|run| {
                    if let Some(start) = &mut run.start {
                        start.fingerprint = Some(fingerprint);
                    }
                    Ok(())
                })
            },
            &self.context,
        );
        self.jobs
            .push(async move { Event::Fingerprint(pending.await) }.boxed());
    }
    fn on_call(&mut self, call: HostCall) {
        if self.ended.is_some() || self.failure.is_some() || self.caller_aborted {
            return;
        }
        if call.target == CallTarget::Tool {
            let name = if call
                .name
                .units()
                .starts_with(&CODEMODE_INTERNAL_PREFIX.encode_utf16().collect::<Vec<_>>())
            {
                JsonString::from_utf16(call.name.units()[CODEMODE_INTERNAL_PREFIX.len()..].to_vec())
            } else {
                call.name.clone()
            };
            self.rows
                .entry(call.id)
                .or_insert_with(|| CodemodeNestedCall {
                    id: call.id,
                    name,
                    args: cut(
                        call.args
                            .as_ref()
                            .map(|v| v.to_string().into())
                            .unwrap_or_default(),
                        200,
                    ),
                    status: "running".into(),
                    duration_ms: None,
                    error: None,
                });
        }
        self.issued = self.issued.max(call.id);
        let index = if self.replaying {
            self.replay_index.get(&call.id).copied()
        } else {
            None
        };
        let Some(index) = index else {
            if matches!(self.release, Release::Live) {
                self.waiting.push(call);
                self.launch_covered();
            } else {
                self.held.push(call);
            }
            return;
        };
        let entry = &self.replayed[index];
        if let Some(global) = &entry.global {
            if call.target != CallTarget::Global
                || call.name != global.name
                || call.args != global.args
            {
                self.diverge(join(
                    [
                        "call ".into(),
                        call.id.to_string().into(),
                        " is ".into(),
                        describe_call(&call),
                        ", but was a call to ".into(),
                        global.name.clone(),
                    ],
                    "",
                ));
                return;
            }
            match Outcome::from_json(global.outcome.value()) {
                Ok(outcome) => {
                    self.ready.insert(call.id, outcome);
                    if self.gate
                        && let Err(error) = self.deliver_replayed()
                    {
                        self.fail(error);
                    }
                }
                Err(error) => self.fail(error),
            }
            return;
        }
        if call.target != CallTarget::Tool {
            self.diverge(join(
                [
                    "call ".into(),
                    call.id.to_string().into(),
                    " is ".into(),
                    describe_call(&call),
                    ", but was a tool call".into(),
                ],
                "",
            ));
            return;
        }
        if self.gate {
            self.start_tool(call, true);
        } else {
            self.model_waiters.push_back((call, true));
        }
    }
    fn waiting_replay(&mut self) -> Vec<HostCall> {
        let mut calls = vec![];
        let mut keep = VecDeque::new();
        while let Some((call, replay)) = self.model_waiters.pop_front() {
            if replay {
                calls.push(call);
            } else {
                keep.push_back((call, replay));
            }
        }
        self.model_waiters = keep;
        calls
    }
    fn deliver_replayed(&mut self) -> Result<()> {
        if !self.gate {
            return Ok(());
        }
        while self.next_replayed < self.replayed.len() && !self.stopped() {
            let entry = &self.replayed[self.next_replayed];
            if entry.id > self.issued {
                return Ok(());
            }
            let Some(outcome) = self.ready.remove(&entry.id) else {
                return Ok(());
            };
            self.next_replayed += 1;
            self.run
                .deliver(self.next_replayed as u64, entry.at, entry.id, outcome)?;
        }
        Ok(())
    }
    fn on_idle(&mut self, seq: u64, issued: u64) {
        if self.stopped() {
            return;
        }
        if self.rerun && seq <= self.replayed.len() as u64 {
            let recorded = if seq == 0 {
                self.first_issued
            } else {
                self.replayed[seq as usize - 1].issued_after
            };
            if let Some(recorded) = recorded
                && recorded != issued
            {
                self.diverge(if seq==0 {format!("its first run made {issued} calls, where it had made {recorded}").into()}else{format!("after result {seq} it had made {issued} calls, where it had made {recorded}").into()});
                return;
            }
        }
        self.acks.insert(seq, issued);
        if !self.replaying {
            if seq == 0 && self.rerun {
                self.release();
            }
            if seq == self.journal_length as u64 {
                self.pump();
            }
            return;
        }
        if seq != self.next_replayed as u64 {
            return;
        }
        if let Some(next) = self.replayed.get(seq as usize) {
            if next.id > issued {
                self.diverge(
                    format!(
                        "it waits without making call {}, which it made before",
                        next.id
                    )
                    .into(),
                );
            }
            return;
        }
        self.replaying = false;
        self.pump();
        self.release();
    }
    fn release(&mut self) {
        if !self.gate || self.stopped() || !matches!(self.release, Release::Held) {
            return;
        }
        self.release = Release::Checking;
        let calls = std::mem::take(&mut self.held);
        let api = self.api.clone();
        let context = self.context.clone();
        let rerun = self.rerun;
        let pending = eager(async move {
            let mut mismatch = None;
            if rerun {
                for call in &calls {
                    if let Some(other) = earlier(&api, &context, call).await? {
                        mismatch = Some((call.clone(), other));
                        break;
                    }
                }
            }
            Ok((calls, mismatch))
        });
        self.jobs.push(
            async move {
                match pending.await {
                    Ok((calls, mismatch)) => Event::Released(calls, Ok(mismatch)),
                    Err(error) => Event::Released(vec![], Err(error)),
                }
            }
            .boxed(),
        );
    }
    fn launch_covered(&mut self) {
        if !matches!(self.release, Release::Live) || self.stopped() {
            return;
        }
        let mut covered = vec![];
        self.waiting.retain(|call| {
            if call.id <= self.durable_issued {
                covered.push(call.clone());
                false
            } else {
                true
            }
        });
        for call in covered {
            if call.target == CallTarget::Tool {
                self.start_tool(call, false);
            } else {
                self.start_global(call);
            }
        }
    }
    fn start_tool(&mut self, call: HostCall, replay: bool) {
        let model = call
            .name
            .units()
            .starts_with(&CODEMODE_INTERNAL_PREFIX.encode_utf16().collect::<Vec<_>>());
        if model && self.model_calls >= MAX_CONCURRENT_MODEL_CALLS {
            self.model_waiters.push_back((call, replay));
            return;
        }
        if self.stopped() {
            return;
        }
        if model {
            self.model_calls += 1;
        }
        let pending = self.api.execute_tool_value(
            call.name.clone(),
            arguments(&call),
            &self.context,
            NestedCallOptions {
                key: Some(format!("c{}", call.id).into()),
                progress: None,
            },
        );
        let structured = self
            .start
            .tools
            .iter()
            .find(|tool| tool.name == call.name)
            .is_some_and(|tool| tool.structured);
        let name = call.name.clone();
        self.jobs.push(
            async move {
                Event::ToolDone(
                    call,
                    replay,
                    Box::new(pending.await.map(|result| ToolDone {
                        outcome: tool_outcome(&name, structured, &result),
                        result,
                    })),
                )
            }
            .boxed(),
        );
    }
    fn next_model(&mut self) {
        if !self.stopped()
            && let Some((call, replay)) = self.model_waiters.pop_front()
        {
            self.start_tool(call, replay);
        }
    }
    fn start_global(&mut self, call: HostCall) {
        let pending = if DISCOVERY
            .iter()
            .any(|name| call.name.units() == JsonString::from(*name).units())
        {
            let result = discover(&self.start, &call);
            eager(async move {
                Ok(result
                    .map(Outcome::Success)
                    .unwrap_or_else(|error| Outcome::failure(error.message_text())))
            })
        } else if CATALOG_GLOBALS
            .iter()
            .any(|name| call.name.units() == JsonString::from(*name).units())
        {
            let service = self.api.model_collection().and_then(|models| {
                models
                    .codemode_models()
                    .ok_or_else(|| Error::message("Model collection does not expose typed models"))
            });
            let context = self.context.clone();
            let name = call.name.to_string_lossy();
            let args = call.args.clone();
            eager(async move {
                let result = match service {
                    Ok(service) => catalog_call(&name, args, service, context.clone()).await,
                    Err(error) => Err(error),
                };
                match result {
                    Ok(value) => Ok(Outcome::Success(value)),
                    Err(error) if context.check().is_err() => Err(error),
                    Err(error) => Ok(Outcome::failure(error.message_text())),
                }
            })
        } else if let Some(global) = self
            .options
            .globals
            .iter()
            .find(|global| global.name == call.name)
        {
            let operation =
                (global.execute)(call.args.clone(), self.api.clone(), self.context.clone());
            let context = self.context.clone();
            eager(async move {
                match operation.await {
                    Ok(value) => Ok(Outcome::Success(value)),
                    Err(error) if context.check().is_err() => Err(error),
                    Err(error) => Ok(Outcome::failure(error.message_text())),
                }
            })
        } else {
            let message = join(["Unknown global ".into(), call.name.clone()], "");
            eager(async move { Ok(Outcome::failure(message)) })
        };
        self.jobs
            .push(async move { Event::GlobalDone(call, pending.await) }.boxed());
    }
    fn failed_call(&mut self, call: HostCall, error: Error) {
        if self.stopped() {
            return;
        }
        if self.context.check().is_err() {
            self.fail(error);
        } else {
            self.diverge(join(
                [
                    "call ".into(),
                    call.id.to_string().into(),
                    " is ".into(),
                    describe_call(&call),
                    ": ".into(),
                    error.message_text(),
                ],
                "",
            ));
        }
    }
    fn finish_row(
        &mut self,
        call: &HostCall,
        outcome: &Outcome,
        result: &NestedToolExecutionResult,
    ) {
        if call.name.units() == JsonString::from(GENERATE_IMAGES_TOOL).units() {
            self.generated_images += result
                .structured_output
                .as_ref()
                .and_then(|v| v["output"].as_array())
                .map_or(0, |items| {
                    items
                        .iter()
                        .filter(|item| item["type"].string_equals("image"))
                        .count()
                });
        }
        if let Some(row) = self.rows.get_mut(&call.id) {
            row.status = if result.is_error {
                "error".into()
            } else {
                "ok".into()
            };
            row.duration_ms = result.duration_ms;
            let error = match outcome {
                Outcome::Failure { message, .. } => message.clone(),
                _ => join(
                    result
                        .diagnostics
                        .iter()
                        .filter(|d| d.severity == DiagnosticSeverity::Error)
                        .map(|d| d.message.clone()),
                    "\n",
                ),
            };
            if result.is_error && !error.units().is_empty() {
                row.error = Some(cut(error, 500));
            }
        }
    }
    fn pump(&mut self) {
        if self.committing || self.stopped() || self.queue.is_empty() && self.acks.is_empty() {
            return;
        }
        self.committing = true;
        let batch = std::mem::take(&mut self.queue);
        let acks: Vec<_> = std::mem::take(&mut self.acks).into_iter().collect();
        let saved = batch.clone();
        let counts = acks.clone();
        let task = self.api.task_id().untyped();
        let pending = self.api.commit(
            move |tx| async move {
                tx.doc(&CODEMODE_JOURNAL_DOC, task)
                    .await?
                    .update(|journal| {
                        for (seq, issued) in counts {
                            if seq == 0 {
                                journal.first_issued.get_or_insert(issued);
                            } else if let Some(entry) = journal.entries.get_mut(seq as usize - 1) {
                                entry.issued_after.get_or_insert(issued);
                            }
                        }
                        for item in saved {
                            journal.entries.push(JournalEntry {
                                id: item.call.id,
                                at: item.at,
                                global: item.global,
                                issued_after: None,
                            });
                        }
                        Ok(())
                    })
            },
            &self.context,
        );
        self.jobs
            .push(async move { Event::Pumped(batch, acks, pending.await) }.boxed());
    }
    async fn finish(&mut self, result: JsonValue) -> Result<ToolExecutionResult> {
        if let Some(error) = self.failure.take() {
            return Err(error);
        }
        if let Some(result) = self.ended.take() {
            return Ok(result);
        }
        self.context.check()?;
        let ok: bool = wire::field(&result, "ok").map_err(Error::message)?;
        if !ok
            && result["error"]["kind"].string_equals("aborted")
            && !self.controller.signal().aborted()
        {
            if let Some(signal) = self.context.abort_signal() {
                signal.cancelled().await;
                self.context.check()?;
            }
            return Err(Error::message(
                "The code mode sandbox was closed while the script ran",
            ));
        }
        if self.rerun
            && let Some(during) = wire::get(&result, "during")
        {
            let seq: u64 = wire::field(during, "seq").map_err(Error::message)?;
            let issued: u64 = wire::field(during, "issued").map_err(Error::message)?;
            if seq <= self.replayed.len() as u64 {
                let continued = if seq == 0 {
                    !self.replayed.is_empty() || self.first_issued.is_some()
                } else {
                    self.replayed[seq as usize - 1].issued_after.is_some()
                };
                if continued
                    || self
                        .replayed
                        .iter()
                        .skip(seq as usize)
                        .any(|entry| entry.id > issued)
                {
                    return decide(&self.api,&self.context,error_result(format!("The script took a different path when resumed after a restart: it ended after result {seq}, where it went on before.")),details(&self.rows)).await;
                }
            }
        }
        let mut details = details(&self.rows);
        let formatted = format_result(
            result.clone(),
            FormatResultOptions {
                generated_images: self.generated_images,
                wall_ms: self.api.now()? - self.start.started_at,
                max_output_tokens: self.start.max_output_tokens,
                calls: details
                    .calls
                    .iter()
                    .map(|call| CallSummary {
                        name: call.name.clone(),
                        status: call.status.clone(),
                    })
                    .collect(),
                max_bytes: OUTPUT_BYTES as f64,
                max_lines: OUTPUT_LINES as f64,
                write: output_files(self.api.clone(), self.context.clone()),
            },
        )
        .await?;
        details.full_output_path = formatted.full_output_path;
        let tool = ToolExecutionResult {
            output: Some(formatted.content),
            is_error: Some(!ok),
            details: Some(wire::encode(&details).into()),
            ..Default::default()
        };
        let task = self.api.task_id().untyped();
        let conversation = self.api.conversation_id();
        let saved = tool.clone();
        let writes = ok
            .then(|| wire::get(&result, "storeWrites").cloned())
            .flatten();
        self.api
            .commit(
                move |tx| async move {
                    let run = tx.doc(&CODEMODE_RUN_DOC, task).await?;
                    if let Some(result) = run.read()?.result {
                        return wire::decode(result.value()).map_err(Error::message);
                    }
                    run.update(|run| {
                        run.result = Some(
                            JsonObject::try_from_value(wire::encode(&saved))
                                .map_err(Error::message)?,
                        );
                        Ok(())
                    })?;
                    if let Some(writes) = writes {
                        let set: JsonObject =
                            wire::field(&writes, "set").map_err(Error::message)?;
                        let delete: Vec<JsonString> =
                            wire::field(&writes, "delete").map_err(Error::message)?;
                        if !set.is_empty() || !delete.is_empty() {
                            tx.doc(&CODEMODE_STORE_DOC, conversation)
                                .await?
                                .update(|store| {
                                    for key in delete {
                                        store.values.shift_remove(&key);
                                    }
                                    for (key, value) in set.iter() {
                                        store.values.insert(key.clone(), value.clone());
                                    }
                                    Ok(())
                                })?;
                        }
                    }
                    Ok(saved)
                },
                &self.context,
            )
            .await
    }
}
async fn earlier(
    api: &ToolExecutionApi,
    context: &Context,
    call: &HostCall,
) -> Result<Option<JsonString>> {
    let state = api
        .snapshot_family(
            &NESTED_CALL_DOC,
            api.task_id().untyped(),
            format!("c{}", call.id),
            context,
        )
        .await?;
    let Some(task) = state.and_then(|state| state.task_id) else {
        return Ok(None);
    };
    let Some(record) = api.get_task(task, context).await? else {
        return Ok(None);
    };
    let Ok(ToolTaskInput::Nested { call: made, .. }) =
        wire::decode::<ToolTaskInput>(&record.info.input)
    else {
        return Ok(None);
    };
    if call.target == CallTarget::Tool
        && made.name == call.name
        && made.arguments == arguments(call)
    {
        return Ok(None);
    }
    Ok(Some(join(
        [
            "tools.".into(),
            made.name,
            "(".into(),
            made.arguments.to_string().into(),
            ")".into(),
        ],
        "",
    )))
}
fn output_files(api: ToolExecutionApi, context: Context) -> OutputFile {
    let directory = Arc::new(parking_lot::Mutex::new(
        None::<futures::future::Shared<BoxFuture<'static, Result<JsonString>>>>,
    ));
    Arc::new(move |name, data| {
        let Some(env) = api.env.clone() else {
            return Box::pin(async {
                Ok(OutputFileResult::Failed("no execution environment".into()))
            });
        };
        let mut slot = directory.lock();
        let directory = slot
            .get_or_insert_with(|| {
                let api = api.clone();
                let context = context.clone();
                let env = env.clone();
                eager(async move {
                    let task = api.task_id().untyped();
                    let stored = api
                        .commit(
                            move |tx| async move {
                                Ok(tx.doc(&CODEMODE_RUN_DOC, task).await?.read()?.output_dir)
                            },
                            &context,
                        )
                        .await?;
                    if let Some(path) = stored {
                        return Ok(path);
                    }
                    let created = env
                        .create_temp_dir(Some("pi-codemode-"), &context)
                        .await
                        .map_err(Error::file)?;
                    let created = created.to_string_lossy().into_owned().into();
                    api.commit(
                        move |tx| async move {
                            tx.doc(&CODEMODE_RUN_DOC, task)
                                .await?
                                .update(|run| Ok(run.output_dir.get_or_insert(created).clone()))
                        },
                        &context,
                    )
                    .await
                })
                .boxed()
                .shared()
            })
            .clone();
        drop(slot);
        let context = context.clone();
        Box::pin(async move {
            let outcome = async {
                let path = join([directory.await?, "/".into(), name.into()], "");
                let native = std::path::PathBuf::from(path.to_string_lossy());
                match data {
                    OutputData::Text(text) => env
                        .write_text_file(&native, &text, &context)
                        .await
                        .map_err(Error::file)?,
                    OutputData::Binary(bytes) => env
                        .write_file(&native, &bytes, &context)
                        .await
                        .map_err(Error::file)?,
                }
                Ok::<_, Error>(path)
            }
            .await;
            Ok(match outcome {
                Ok(path) => OutputFileResult::Saved(path),
                Err(error) => OutputFileResult::Failed(error.message_text()),
            })
        })
    })
}
fn discover(start: &Start, call: &HostCall) -> Result<Option<JsonValue>> {
    let args = call
        .args
        .as_ref()
        .and_then(JsonValue::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let tools: Vec<_> = start.tools.iter().filter(|tool| !tool.hidden).collect();
    let matches = |namespace: &StartNamespace, query: &JsonString| {
        namespace.name == *query || identifier(&namespace.name) == identifier(query)
    };
    match call.name.to_string_checked().ok().as_deref() {
        Some("describeTool") => {
            let name = args
                .first()
                .and_then(JsonValue::as_string)
                .ok_or_else(|| Error::message("describeTool() expects a tool name"))?;
            Ok(tools
                .iter()
                .find(|tool| tool.name == *name || tool.js_name == *name)
                .map(|tool| wire::encode(&tool.description)))
        }
        Some("describeNamespace") => {
            let name = args
                .first()
                .and_then(JsonValue::as_string)
                .ok_or_else(|| Error::message("describeNamespace() expects a namespace name"))?;
            Ok(start
                .namespaces
                .iter()
                .find(|namespace| matches(namespace, name))
                .map(|namespace| {
                    let mut value = wire::object();
                    wire::put(&mut value, "name", &namespace.name);
                    wire::put_optional(&mut value, "description", &namespace.description);
                    wire::put_optional(&mut value, "instructions", &namespace.instructions);
                    wire::put(
                        &mut value,
                        "tools",
                        &namespace.tools.iter().map(identifier).collect::<Vec<_>>(),
                    );
                    value
                }))
        }
        _ => {
            let query = args
                .first()
                .and_then(JsonValue::as_string)
                .ok_or_else(|| Error::message("searchTools() expects a query string"))?;
            let options = args.get(1);
            let limit = options
                .and_then(|o| wire::get(o, "limit"))
                .filter(|v| !v.is_null())
                .map_or(Some(8.0), JsonValue::as_f64)
                .ok_or_else(|| Error::message("searchTools() limit must be a positive integer"))?;
            if !limit.is_finite() || limit.fract() != 0.0 || limit <= 0.0 {
                return Err(Error::message(
                    "searchTools() limit must be a positive integer",
                ));
            }
            let namespace = options
                .and_then(|o| wire::get(o, "namespace"))
                .filter(|v| !v.is_null());
            if namespace.is_some_and(|value| !value.is_string()) {
                return Err(Error::message("searchTools() namespace must be a string"));
            }
            let documents: Vec<_> = tools
                .iter()
                .filter_map(|tool| {
                    let own = start
                        .namespaces
                        .iter()
                        .find(|namespace| namespace.tools.contains(&tool.name));
                    if let Some(namespace) = namespace.and_then(JsonValue::as_string)
                        && own.is_none_or(|own| !matches(own, namespace))
                    {
                        return None;
                    }
                    let spaced = JsonString::from_utf16(
                        tool.name
                            .units()
                            .iter()
                            .map(|&u| if u == 95 { 32 } else { u })
                            .collect(),
                    );
                    Some(crate::search::SearchDocument {
                        name: tool.name.clone(),
                        text: join(
                            [
                                tool.name.clone(),
                                spaced,
                                tool.description.clone(),
                                own.map(|n| n.name.clone()).unwrap_or_default(),
                                own.and_then(|n| n.description.clone()).unwrap_or_default(),
                            ],
                            " ",
                        ),
                    })
                })
                .collect();
            Ok(Some(JsonValue::Array(
                crate::search::rank_documents(query, &documents, limit)
                    .into_iter()
                    .map(|name| {
                        let tool = tools.iter().find(|tool| tool.name == name).unwrap();
                        let mut value = wire::object();
                        wire::put(&mut value, "name", &tool.js_name);
                        wire::put(&mut value, "description", &tool.description);
                        value
                    })
                    .collect::<Vec<_>>()
                    .into(),
            )))
        }
    }
}
