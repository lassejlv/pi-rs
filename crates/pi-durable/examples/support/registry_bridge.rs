//! Opaque original JS registrations are host capabilities. Rust owns registry
//! validation, publication, selection, wrappers, filters and hook ordering.
use super::*;
use pi_durable::harness::reader::{ReaderSubscription, RegistryReader, RegistryView};
use pi_durable::harness::{agent::*, registry::*, settings::*};
use serde::Deserialize;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ToolHandle {
    pub(super) id: u64,
    name: JsonString,
    callers: Option<Vec<ToolCaller>>,
    #[serde(default)]
    deferred: bool,
    #[serde(skip)]
    pub executable: Option<Arc<dyn pi_durable::harness::tool::ExecutableTool>>,
}
impl RegisteredTool for ToolHandle {
    fn name(&self) -> &JsonString {
        &self.name
    }
    fn callers(&self) -> Option<&[ToolCaller]> {
        self.callers.as_deref()
    }
    fn deferred(&self) -> bool {
        self.deferred
    }
    fn executable(self: Arc<Self>) -> Option<Arc<dyn pi_durable::harness::tool::ExecutableTool>> {
        self.executable.clone()
    }
}
#[derive(Serialize, Deserialize)]
pub(super) struct SectionHandle {
    pub(super) id: u64,
    pub(super) key: JsonString,
    #[serde(default)]
    pub(super) untagged: bool,
    #[serde(skip)]
    pub(super) hub: std::sync::Weak<Hub>,
}
impl RegisteredSection for SectionHandle {
    fn key(&self) -> &JsonString {
        &self.key
    }
}
type Ext = Extension<ToolHandle, SectionHandle>;
type Snapshot = RegistrySnapshot<ToolHandle, SectionHandle>;
type ResolvedAgent = Agent<ToolHandle, SectionHandle>;

#[derive(Default)]
pub(super) struct RegistryBridge {
    registries: Mutex<HashMap<u64, Registry<ToolHandle, SectionHandle>>>,
    extensions: Mutex<HashMap<u64, Arc<Ext>>>,
    snapshots: Mutex<HashMap<u64, Snapshot>>,
    agents: Mutex<HashMap<u64, Arc<ResolvedAgent>>>,
    subscriptions: Mutex<HashMap<u64, RegistrySubscription<ToolHandle, SectionHandle>>>,
    operations: AtomicU64,
    resolutions: AtomicU64,
    wrappers: AtomicU64,
    reader_listeners: Mutex<HashMap<u64, ReaderListener>>,
    pub reader_snapshots: AtomicU64,
    pub reader_subscriptions: AtomicU64,
}
type ReaderListener = Arc<dyn Fn() + Send + Sync>;
struct HostReader {
    hub: Arc<Hub>,
    handle: u64,
}
impl RegistryReader for HostReader {
    fn snapshot(&self) -> Result<RegistryView> {
        self.hub
            .registry
            .reader_snapshots
            .fetch_add(1, Ordering::Relaxed);
        let snapshot = self.hub.sync("readerSnapshot", encode(&self.handle))?;
        if let Some(tasks) = snapshot["tasks"].as_array() {
            let catalog = pi_durable::harness::catalog::TaskCatalog::default();
            catalog.replace(
                tasks
                    .iter()
                    .map(|task| {
                        Ok(pi_durable::harness::catalog::InstalledTask {
                            task: self.hub.stock_catalog_task(task)?,
                            extension: None,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            )?;
            return Ok(RegistryView::from_catalog(catalog.snapshot()));
        }
        let id: u64 = decode(&snapshot)?;
        self.hub
            .registry
            .snapshots
            .lock()
            .get(&id)
            .cloned()
            .map(Into::into)
            .ok_or_else(|| Error::message("Custom reader returned an unknown native snapshot"))
    }
    fn subscribe(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Result<ReaderSubscription> {
        self.hub
            .registry
            .reader_subscriptions
            .fetch_add(1, Ordering::Relaxed);
        let id = self.hub.next.fetch_add(1, Ordering::Relaxed);
        self.hub
            .registry
            .reader_listeners
            .lock()
            .insert(id, listener);
        let subscription = self.hub.sync("readerSubscribe", encode(&(self.handle, id)));
        let subscription: u64 = match subscription.and_then(|value| decode(&value)) {
            Ok(id) => id,
            Err(error) => {
                self.hub.registry.reader_listeners.lock().remove(&id);
                return Err(error);
            }
        };
        let hub = self.hub.clone();
        Ok(ReaderSubscription::new(move || {
            hub.registry.reader_listeners.lock().remove(&id);
            hub.sync("readerUnsubscribe", encode(&subscription))?;
            Ok(())
        }))
    }
}
impl Hub {
    pub(super) fn registry_reader(
        self: &Arc<Self>,
        request: &JsonValue,
    ) -> Result<Arc<dyn RegistryReader>> {
        if let Some(handle) = request["reader"].as_u64() {
            Ok(Arc::new(HostReader {
                hub: self.clone(),
                handle,
            }))
        } else {
            Ok(Arc::new(self.registry_by_id(field(request, "registry")?)?))
        }
    }
    pub(super) fn registry_by_id(&self, id: u64) -> Result<Registry<ToolHandle, SectionHandle>> {
        self.registry
            .registries
            .lock()
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::message("Unknown test registry"))
    }
    pub(super) fn runtime_registry(&self, request: &JsonValue) -> Result<u64> {
        self.registry_reads.fetch_add(1, Ordering::Relaxed);
        let snapshot = self
            .runtime(request)?
            .registry()
            .typed::<ToolHandle, SectionHandle>()?;
        Ok(self.snapshot_handle(snapshot))
    }
    pub(super) fn encode_agent(&self, agent: Arc<ResolvedAgent>) -> JsonValue {
        let existing = self
            .registry
            .agents
            .lock()
            .iter()
            .find(|(_, held)| Arc::ptr_eq(held, &agent))
            .map(|(id, _)| *id);
        let id = existing.unwrap_or_else(|| self.next.fetch_add(1, Ordering::Relaxed));
        let mut value = object([
            ("id", encode(&id)),
            ("thinkingLevel", encode(&agent.thinking_level)),
            (
                "extensions",
                encode(
                    &agent
                        .extensions
                        .iter()
                        .map(|e| self.extension_id(e))
                        .collect::<Vec<_>>(),
                ),
            ),
            (
                "tools",
                encode(&agent.tools.iter().map(|t| t.id).collect::<Vec<_>>()),
            ),
            (
                "callable",
                encode(&agent.callable.iter().map(|t| t.id).collect::<Vec<_>>()),
            ),
            (
                "deferred",
                encode(&agent.deferred.iter().map(|t| t.id).collect::<Vec<_>>()),
            ),
            (
                "sections",
                encode(
                    &agent
                        .sections
                        .iter()
                        .map(|s| match s {
                            AgentSection::Registered(s) => object([("id", encode(&s.id))]),
                            AgentSection::Instructions(text) => {
                                object([("instructions", encode(text))])
                            }
                        })
                        .collect::<Vec<_>>(),
                ),
            ),
        ]);
        let fields = value.as_object_mut().unwrap();
        if let Some(model) = &agent.model {
            fields.insert("model".into(), model.clone());
        }
        if let Some(instructions) = &agent.instructions {
            fields.insert("instructions".into(), encode(instructions));
        }
        if let Some(cwd) = &agent.cwd {
            fields.insert("cwd".into(), encode(cwd));
        }
        self.registry.agents.lock().insert(id, agent);
        value
    }
    fn extension_id(&self, extension: &Arc<Ext>) -> u64 {
        *self
            .registry
            .extensions
            .lock()
            .iter()
            .find(|(_, held)| Arc::ptr_eq(held, extension))
            .unwrap()
            .0
    }
    fn snapshot_handle(&self, snapshot: Snapshot) -> u64 {
        let mut snapshots = self.registry.snapshots.lock();
        if let Some((id, _)) = snapshots
            .iter()
            .find(|(_, held)| held.same_snapshot(&snapshot))
        {
            return *id;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        snapshots.insert(id, snapshot);
        id
    }
    fn registry_extension(self: &Arc<Self>, description: &JsonValue) -> Result<Arc<Ext>> {
        let id: u64 = field(description, "id")?;
        // Reinstalling the same registration must also preserve old snapshots'
        // mapping back to that original JS object.
        if let Some(extension) = self.registry.extensions.lock().get(&id).cloned() {
            return Ok(extension);
        }
        let mut extension = Ext::new(field::<JsonString>(description, "name")?);
        extension.tools = field::<Vec<JsonValue>>(description, "tools")?
            .iter()
            .map(|value| self.host_tool(value))
            .collect::<Result<_>>()?;
        extension.sections = field::<Vec<SectionHandle>>(description, "sections")?
            .into_iter()
            .map(|mut section| {
                section.hub = Arc::downgrade(self);
                Arc::new(section)
            })
            .collect();
        extension.tasks = field::<Vec<JsonValue>>(description, "tasks")?
            .iter()
            .map(|description| self.stock_catalog_task(description))
            .collect::<Result<_>>()?;
        for hook in field::<Vec<JsonValue>>(description, "hooks")? {
            if hook["nativeCompaction"].as_bool() == Some(true) {
                extension.hooks.push(HookRegistration::new(
                    "pi.compaction",
                    Arc::new(pi_durable::harness::compaction::CompactionHooks {
                        provider: Some(Arc::new(super::compaction_bridge::HostCompactionHooks {
                            hub: Arc::downgrade(self),
                            id: field(&hook, "handlers")?,
                        })),
                        ..Default::default()
                    }),
                ));
            }
            if hook["nativeGeneration"].as_bool() == Some(true) {
                extension.hooks.push(HookRegistration::new(
                    "pi.generation",
                    Arc::new(pi_durable::harness::generation::GenerationHooks {
                        provider: Some(Arc::new(super::generation_bridge::HostGenerationHooks {
                            hub: Arc::downgrade(self),
                            id: field(&hook, "handlers")?,
                        })),
                        ..Default::default()
                    }),
                ));
            }
            if hook["nativeTool"].as_bool() == Some(true) {
                extension.hooks.push(HookRegistration::new(
                    "pi.tool",
                    Arc::new(pi_durable::harness::tool::ToolHooks {
                        provider: Some(Arc::new(super::tool_bridge::HostToolHooks {
                            hub: Arc::downgrade(self),
                            id: field(&hook, "handlers")?,
                        })),
                        ..Default::default()
                    }),
                ));
            }
            extension.hooks.push(HookRegistration::new(
                field::<JsonString>(&hook, "task")?,
                Arc::new(field::<u64>(&hook, "handlers")?),
            ));
        }
        for wrap in field::<Vec<JsonValue>>(description, "wraps")? {
            let callback: u64 = field(&wrap, "callback")?;
            let hub = self.clone();
            if let Some(name) = wrap["tool"].as_string() {
                extension.wraps.push(Wrap::Tool {
                    name: name.clone(),
                    apply: Arc::new(move |tool| {
                        hub.registry.wrappers.fetch_add(1, Ordering::Relaxed);
                        hub.host_tool(&hub.sync(
                            "registry.wrap",
                            object([
                                ("callback", encode(&callback)),
                                ("kind", encode(&"tool")),
                                ("item", encode(&tool.id)),
                            ]),
                        )?)
                    }),
                });
            } else {
                extension.wraps.push(Wrap::Section {
                    key: field(&wrap, "section")?,
                    apply: Arc::new(move |section| {
                        hub.registry.wrappers.fetch_add(1, Ordering::Relaxed);
                        let mut section: SectionHandle = decode(&hub.sync(
                            "registry.wrap",
                            object([
                                ("callback", encode(&callback)),
                                ("kind", encode(&"section")),
                                ("item", encode(&section.id)),
                            ]),
                        )?)?;
                        section.hub = Arc::downgrade(&hub);
                        Ok(Arc::new(section))
                    }),
                });
            }
        }
        let extension = Arc::new(extension);
        self.registry
            .extensions
            .lock()
            .insert(id, extension.clone());
        Ok(extension)
    }
    pub(super) fn registry_request(
        self: &Arc<Self>,
        method: &str,
        request: JsonValue,
    ) -> Result<Option<JsonValue>> {
        self.registry.operations.fetch_add(1, Ordering::Relaxed);
        let registry = || {
            self.registry
                .registries
                .lock()
                .get(&field::<u64>(&request, "registry")?)
                .cloned()
                .ok_or_else(|| Error::message("Unknown test registry"))
        };
        let snapshot = || {
            self.registry
                .snapshots
                .lock()
                .get(&field::<u64>(&request, "snapshot")?)
                .cloned()
                .ok_or_else(|| Error::message("Unknown test snapshot"))
        };
        let agent = || {
            self.registry
                .agents
                .lock()
                .get(&field::<u64>(&request, "agent")?)
                .cloned()
                .ok_or_else(|| Error::message("Unknown test agent"))
        };
        Ok(Some(match method {
            "registry.notifyReader" => {
                let listener = self
                    .registry
                    .reader_listeners
                    .lock()
                    .get(&field(&request, "listener")?)
                    .cloned();
                if let Some(listener) = listener {
                    listener();
                }
                JsonValue::Null
            }
            "registry.create" => {
                let builtins = field::<Vec<JsonValue>>(&request, "builtins")?
                    .iter()
                    .map(|definition| self.stock_catalog_task(definition))
                    .collect::<Result<Vec<_>>>()?;
                let id = self.next.fetch_add(1, Ordering::Relaxed);
                self.registry
                    .registries
                    .lock()
                    .insert(id, Registry::new(builtins)?);
                encode(&id)
            }
            "registry.install" => {
                let registry = registry()?;
                let extension = self.registry_extension(&request["extension"])?;
                registry.install(extension)?;
                JsonValue::Null
            }
            "registry.uninstall" => {
                registry()?.uninstall(&field(&request, "name")?)?;
                JsonValue::Null
            }
            "registry.snapshot" => encode(&self.snapshot_handle(registry()?.snapshot())),
            "registry.subscribe" => {
                let registry = registry()?;
                let callback: u64 = field(&request, "callback")?;
                let hub = self.clone();
                let subscription = registry.subscribe_fallible(move || {
                    hub.sync("registry.listener", encode(&callback))?;
                    Ok(())
                });
                let id = self.next.fetch_add(1, Ordering::Relaxed);
                self.registry.subscriptions.lock().insert(id, subscription);
                encode(&id)
            }
            "registry.unsubscribe" => {
                self.registry
                    .subscriptions
                    .lock()
                    .remove(&field::<u64>(&request, "subscription")?);
                JsonValue::Null
            }
            "registry.installed" => encode(
                &snapshot()?
                    .installed()
                    .iter()
                    .map(|e| self.extension_id(e))
                    .collect::<Vec<_>>(),
            ),
            "registry.extension" => {
                let result = snapshot()?
                    .extension(&field(&request, "name")?)
                    .map(|e| self.extension_id(&e));
                return Ok(result.map(|id| encode(&id)));
            }
            "registry.tools" => encode(
                &snapshot()?
                    .tools()
                    .iter()
                    .map(|(e, t)| {
                        object([
                            ("extension", encode(&self.extension_id(e))),
                            ("tool", encode(&t.id)),
                        ])
                    })
                    .collect::<Vec<_>>(),
            ),
            "registry.sections" => encode(
                &snapshot()?
                    .sections()
                    .iter()
                    .map(|(e, s)| {
                        object([
                            ("extension", encode(&self.extension_id(e))),
                            ("section", encode(&s.id)),
                        ])
                    })
                    .collect::<Vec<_>>(),
            ),
            "registry.tasks" => encode(
                &snapshot()?
                    .tasks()
                    .iter()
                    .map(|task| self.bound_task_id(task))
                    .collect::<Result<Vec<_>>>()?,
            ),
            "registry.task" => {
                return snapshot()?
                    .task(&field(&request, "name")?)
                    .map(|task| self.bound_task_id(&task).map(|id| Some(encode(&id))))
                    .unwrap_or(Ok(None));
            }
            "registry.settings" => settings_value(&settings(&request)?),
            "registry.resolve" => {
                self.registry.resolutions.fetch_add(1, Ordering::Relaxed);
                let state: Option<AgentState> = field(&request, "state")?;
                let report: u64 = field(&request, "report")?;
                let agent = try_resolve_agent(
                    state.as_ref(),
                    &snapshot()?,
                    &settings(&request)?,
                    |error, extension| {
                        self.sync(
                            "registry.report",
                            object([
                                ("callback", encode(&report)),
                                (
                                    "error",
                                    object([
                                        ("name", encode(&error.name())),
                                        ("message", encode(&error.to_string())),
                                    ]),
                                ),
                                ("extension", encode(extension)),
                            ]),
                        )?;
                        Ok(())
                    },
                )?;
                self.encode_agent(Arc::new(agent))
            }
            "registry.hooks" => encode(
                &agent_hooks(agent()?.as_ref(), &field(&request, "task")?)
                    .into_iter()
                    .filter_map(|(extension, handlers)| {
                        handlers.downcast_ref::<u64>().map(|handlers| {
                            object([
                                ("extension", encode(&extension)),
                                ("handlers", encode(handlers)),
                            ])
                        })
                    })
                    .collect::<Vec<_>>(),
            ),
            "registry.sectionExtension" => {
                return Ok(
                    section_extension(agent()?.as_ref(), &field(&request, "key")?)
                        .map(|name| encode(&name)),
                );
            }
            "registry.toolExtension" => {
                return Ok(tool_extension(agent()?.as_ref(), &field(&request, "name")?)
                    .map(|name| encode(&name)));
            }
            "registry.stats" => object([
                (
                    "operations",
                    encode(&self.registry.operations.load(Ordering::Relaxed)),
                ),
                (
                    "resolutions",
                    encode(&self.registry.resolutions.load(Ordering::Relaxed)),
                ),
                (
                    "wrappers",
                    encode(&self.registry.wrappers.load(Ordering::Relaxed)),
                ),
            ]),
            _ => {
                return Err(Error::message(format!(
                    "Unknown registry bridge method {method}"
                )));
            }
        }))
    }
}
pub(super) fn settings(request: &JsonValue) -> Result<Settings> {
    Ok(resolve_settings(settings_input(request)?))
}
pub(super) fn settings_input(request: &JsonValue) -> Result<HarnessSettings> {
    let mut input: HarnessSettings = decode(&request["settings"])?;
    if let Some(numbers) = request["runtimeNumbers"].as_object() {
        for (key, tag) in numbers {
            let number = match tag.to_utf8().as_deref() {
                Some("Infinity") => f64::INFINITY,
                Some("-Infinity") => f64::NEG_INFINITY,
                Some("NaN") => f64::NAN,
                Some("-0") => -0.0,
                _ => return Err(Error::message("Unknown runtime number")),
            };
            match key.to_string().as_str() {
                "contextRetentionMs" => input.context_retention_ms = Some(number),
                "documentRetentionMs" => input.document_retention_ms = Some(number),
                "restartLimit" => input.restart_limit = Some(number),
                "progress.partialIntervalMs" => {
                    input
                        .progress
                        .get_or_insert_with(Default::default)
                        .partial_interval_ms = Some(number)
                }
                "progress.outputIntervalMs" => {
                    input
                        .progress
                        .get_or_insert_with(Default::default)
                        .output_interval_ms = Some(number)
                }
                _ => return Err(Error::message("Unknown runtime number field")),
            }
        }
    }
    Ok(input)
}
pub(super) fn settings_value(settings: &Settings) -> JsonValue {
    fn number(value: f64) -> JsonValue {
        if value.is_finite() && !(value == 0.0 && value.is_sign_negative()) {
            encode(&value)
        } else {
            object([(
                "$nativeNumber",
                encode(&if value == 0.0 {
                    "-0"
                } else if value.is_nan() {
                    "NaN"
                } else if value.is_sign_positive() {
                    "Infinity"
                } else {
                    "-Infinity"
                }),
            )])
        }
    }
    let mut value = encode(settings);
    let fields = value.as_object_mut().unwrap();
    for (key, number_value) in [
        ("contextRetentionMs", settings.context_retention_ms),
        ("documentRetentionMs", settings.document_retention_ms),
        ("restartLimit", settings.restart_limit),
    ] {
        fields.insert(key.into(), number(number_value));
    }
    let progress = fields
        .get_mut(&JsonString::from("progress"))
        .unwrap()
        .as_object_mut()
        .unwrap();
    progress.insert(
        "partialIntervalMs".into(),
        number(settings.progress.partial_interval_ms),
    );
    progress.insert(
        "outputIntervalMs".into(),
        number(settings.progress.output_interval_ms),
    );
    value
}
