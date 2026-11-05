//! Port of `test/suite/agent-session-runtime.test.ts`.

use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_agent::types::{AgentMessage, AgentToolResult};
use pi_ai::auth::types::{ApiKeyCredential, Credential};
use pi_ai::compat::{FauxProviderRegistration, register_faux_provider};
use pi_ai::providers::faux::{FauxModelDefinition, RegisterFauxProviderOptions};
use pi_ai::types::Model;
use pi_coding_agent::core::agent_session::ExtensionBindings;
use pi_coding_agent::core::agent_session_runtime::{
    AgentSessionRuntime, AgentSessionRuntimeForkOptions, AgentSessionRuntimeForkOptionsPosition,
    CreateAgentSessionRuntimeFactory, CreateAgentSessionRuntimeOptions,
    CreateAgentSessionRuntimeResult, create_agent_session_runtime,
};
use pi_coding_agent::core::agent_session_services::{
    CreateAgentSessionFromServicesOptions, CreateAgentSessionServicesOptions,
    create_agent_session_from_services, create_agent_session_services,
};
use pi_coding_agent::core::auth_storage::AuthStorage;
use pi_coding_agent::core::extensions::types::{
    ExtensionAPIRef, ExtensionEvent, ExtensionEventResult, ExtensionFactory, InlineExtension,
    MessageEndEvent, MessageEndEventResult, ProviderConfig, ProviderRegistration, ToolDefinition,
    on_event,
};
use pi_coding_agent::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use pi_coding_agent::core::resource_loader::DefaultResourceLoaderOptions;
use pi_coding_agent::core::session_manager::{SessionEntry, SessionManager};
use pi_agent::types::ThinkingLevel;
use serde_json::{Value, json};

use crate::common::*;
use crate::support::*;

type Setup = Arc<dyn Fn(ExtensionAPIRef) -> anyhow::Result<()> + Send + Sync>;

struct RuntimeTest {
    runtime: Arc<AgentSessionRuntime>,
    faux: FauxProviderRegistration,
    temp_dir: String,
    /// Api-registry guard (see `support::shared_api_registry`).
    _registry_guard: RegistryGuard,
}

enum RegistryGuard {
    Shared(#[allow(dead_code)] tokio::sync::OwnedRwLockReadGuard<()>),
    Exclusive(#[allow(dead_code)] Option<tokio::sync::OwnedRwLockWriteGuard<()>>),
}

impl RuntimeTest {
    async fn cleanup(&self) {
        let _ = self.runtime.dispose().await;
        self.faux.unregister();
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

fn temp_dir(prefix: &str) -> String {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().into_owned()
}

/// Sessions live under the test directory instead of the user's agent dir.
fn session_dir(base: &str) -> String {
    format!("{base}/sessions")
}

async fn model_runtime_for(faux: &FauxProviderRegistration) -> Arc<ModelRuntime> {
    let mut credentials: IndexMap<String, Credential> = IndexMap::new();
    credentials.insert(
        faux.get_model().provider.clone(),
        Credential::ApiKey(ApiKeyCredential {
            key: Some("faux-key".into()),
            env: None,
        }),
    );
    let auth_storage = AuthStorage::in_memory(Some(&credentials));
    ModelRuntime::create(Some(&CreateModelRuntimeOptions {
        credentials: Some(auth_storage),
        models_path: None,
        allow_model_network: Some(false),
        ..Default::default()
    }))
    .await
    .unwrap()
}

/// The runtime factory of the TS test: services per cwd, an inline extension registering the faux
/// provider plus `setup`.
fn factory_for(
    faux: &FauxProviderRegistration,
    agent_dir: &str,
    model_runtime: Arc<ModelRuntime>,
    setup: Option<Setup>,
    model: Option<Model>,
) -> CreateAgentSessionRuntimeFactory {
    let provider = faux.get_model().provider.clone();
    let base_url = faux.get_model().base_url.clone();
    let api = faux.api().to_string();
    let models = provider_models(faux);
    let agent_dir = agent_dir.to_string();
    Arc::new(move |options| {
        let (provider, base_url, api, models) = (provider.clone(), base_url.clone(), api.clone(), models.clone());
        let setup = setup.clone();
        let agent_dir = agent_dir.clone();
        let model_runtime = model_runtime.clone();
        let model = model.clone();
        Box::pin(async move {
            let factory: ExtensionFactory = Arc::new(move |pi: ExtensionAPIRef| {
                let (provider, base_url, api, models) = (provider.clone(), base_url.clone(), api.clone(), models.clone());
                let setup = setup.clone();
                Box::pin(async move {
                    pi.register_provider(ProviderRegistration::Config {
                        name: provider,
                        config: ProviderConfig {
                            base_url: Some(base_url),
                            api_key: Some("faux-key".into()),
                            api: Some(api),
                            models: Some(models),
                            ..Default::default()
                        },
                    })?;
                    if let Some(setup) = setup {
                        setup(pi)?;
                    }
                    Ok(())
                })
            });
            let services = create_agent_session_services(&CreateAgentSessionServicesOptions {
                cwd: options.cwd.clone(),
                agent_dir: Some(agent_dir),
                settings_manager: None,
                model_runtime: Some(model_runtime),
                model_runtime_signal: None,
                extension_flag_values: None,
                resource_loader_options: Some(DefaultResourceLoaderOptions {
                    extension_factories: Some(vec![InlineExtension::Factory(factory)]),
                    no_skills: Some(true),
                    no_prompt_templates: Some(true),
                    no_themes: Some(true),
                    ..Default::default()
                }),
                resource_loader_reload_options: None,
            })
            .await?;
            let created = create_agent_session_from_services(&CreateAgentSessionFromServicesOptions {
                services: services.clone(),
                session_manager: options.session_manager.clone(),
                session_start_event: options.session_start_event.clone(),
                model,
                thinking_level: None,
                scoped_models: None,
                tools: None,
                exclude_tools: None,
                no_tools: None,
                custom_tools: None,
            })
            .await?;
            Ok(CreateAgentSessionRuntimeResult {
                session: created.session,
                extensions_result: created.extensions_result,
                model_fallback_message: created.model_fallback_message,
                diagnostics: services.diagnostics.clone(),
                services,
            })
        })
    })
}

fn register_faux() -> FauxProviderRegistration {
    let faux = register_faux_provider(RegisterFauxProviderOptions {
        models: vec![
            FauxModelDefinition {
                id: "faux-1".into(),
                reasoning: Some(true),
                ..Default::default()
            },
            FauxModelDefinition {
                id: "faux-2".into(),
                reasoning: Some(false),
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    faux.set_responses(vec![text("one"), text("two"), text("three")]);
    faux
}

async fn bind(runtime: &AgentSessionRuntime) {
    runtime
        .session()
        .bind_extensions(&ExtensionBindings::default())
        .await
        .unwrap();
}

async fn create_runtime_for_test(
    setup: Option<Setup>,
    cwd: Option<String>,
    bootstrap_model: bool,
    in_memory: bool,
) -> RuntimeTest {
    let registry_guard = RegistryGuard::Shared(shared_api_registry().await);
    let temp_dir = cwd.unwrap_or_else(|| temp_dir("pi-runtime-suite"));
    let faux = register_faux();
    let model_runtime = model_runtime_for(&faux).await;
    let factory = factory_for(
        &faux,
        &temp_dir,
        model_runtime,
        setup,
        bootstrap_model.then(|| faux.get_model()),
    );
    let session_manager = if in_memory {
        SessionManager::in_memory(Some(&temp_dir), None, None).unwrap()
    } else {
        SessionManager::create(&temp_dir, Some(&session_dir(&temp_dir)), None).unwrap()
    };
    let runtime = create_agent_session_runtime(
        &factory,
        &CreateAgentSessionRuntimeOptions {
            cwd: temp_dir.clone(),
            agent_dir: temp_dir.clone(),
            session_manager,
            session_start_event: None,
        },
    )
    .await
    .unwrap();
    bind(&runtime).await;
    RuntimeTest {
        runtime,
        faux,
        temp_dir,
        _registry_guard: registry_guard,
    }
}

/// Records the named events as JSON; `cancel` decides whether a `session_before_*` event cancels.
fn recorder(
    events: Arc<Mutex<Vec<Value>>>,
    names: &'static [&'static str],
    cancel: Arc<dyn Fn(&Value) -> bool + Send + Sync>,
) -> Setup {
    Arc::new(move |pi| {
        for name in names {
            let events = events.clone();
            let cancel = cancel.clone();
            pi.on(
                name,
                Arc::new(move |event: ExtensionEvent, _ctx| {
                    let events = events.clone();
                    let cancel = cancel.clone();
                    Box::pin(async move {
                        let value = serde_json::to_value(&event).unwrap();
                        let cancelled = cancel(&value);
                        events.lock().push(value);
                        Ok(cancelled.then(|| ExtensionEventResult::Json(json!({"cancel": true}))))
                    })
                }),
            )?;
        }
        Ok(())
    })
}

fn never(_: &Value) -> bool {
    false
}

fn roles_and_user_texts(messages: &[AgentMessage]) -> Vec<(String, Option<String>)> {
    messages
        .iter()
        .map(|m| {
            (
                m.role().to_string(),
                matches!(m, AgentMessage::User(_)).then(|| get_message_text(m)),
            )
        })
        .collect()
}

#[tokio::test]
async fn persists_message_end_assistant_replacements_to_the_session_manager() {
    let setup: Setup = Arc::new(|pi| {
        on_event::<MessageEndEvent, _, _>(&*pi, |event, _ctx| async move {
            let AgentMessage::Assistant(mut message) = event.message else {
                return Ok(None);
            };
            message.usage.cost.total = 0.123;
            Ok(Some(MessageEndEventResult {
                message: Some(AgentMessage::Assistant(message)),
            }))
        })?;
        Ok(())
    });
    let t = create_runtime_for_test(Some(setup), None, true, false).await;
    let session = t.runtime.session();

    session.prompt("hello", None).await.unwrap();

    let assistant = session
        .messages()
        .into_iter()
        .find_map(|m| match m {
            AgentMessage::Assistant(a) => Some(a),
            _ => None,
        })
        .expect("missing assistant message");
    assert_eq!(assistant.usage.cost.total, 0.123);
    let persisted = session
        .session_manager()
        .get_entries()
        .into_iter()
        .find_map(|e| match e {
            SessionEntry::Message(m) => match m.message {
                AgentMessage::Assistant(a) => Some(a),
                _ => None,
            },
            _ => None,
        })
        .expect("missing persisted assistant message");
    assert_eq!(persisted.usage.cost.total, 0.123);
    t.cleanup().await;
}

#[tokio::test]
async fn settles_the_active_response_before_session_replacement() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let started_tx = Arc::new(Mutex::new(Some(started_tx)));
    let setup: Setup = Arc::new(move |pi| {
        let started_tx = started_tx.clone();
        pi.register_tool(ToolDefinition::new(
            "block",
            "Block",
            "Blocks until aborted",
            json!({"type": "object", "properties": {}}),
            move |_id, _params, signal, _update, _ctx| {
                let started_tx = started_tx.clone();
                async move {
                    if let Some(tx) = started_tx.lock().take() {
                        let _ = tx.send(());
                    }
                    if let Some(signal) = signal {
                        signal.cancelled().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                    Ok(AgentToolResult::text("tool aborted", Some(json!({}))))
                }
            },
        ))
    });
    let t = create_runtime_for_test(Some(setup), None, true, false).await;
    t.runtime.session().prompt("hello", None).await.unwrap();
    let first_session_file = t.runtime.session().session_file().unwrap();
    t.runtime.new_session(None).await.unwrap();
    bind(&t.runtime).await;

    t.faux.set_responses(vec![tool_use("block", json!({}))]);
    let outgoing = t.runtime.session();
    let session = outgoing.clone();
    let prompt = tokio::spawn(async move { session.prompt("start blocking tool", None).await });
    started_rx.await.unwrap();

    let switch_result = t.runtime.switch_session(&first_session_file, None).await.unwrap();
    prompt.await.unwrap().unwrap();

    assert!(!switch_result.cancelled);
    assert_eq!(t.runtime.session().session_file().as_deref(), Some(first_session_file.as_str()));
    // The outgoing session settled before replacement: the interrupted tool call has a persisted
    // tool result instead of dangling forever.
    let roles: Vec<String> = SessionManager::open(&outgoing.session_file().unwrap(), None, None)
        .unwrap()
        .get_entries()
        .into_iter()
        .filter_map(|e| match e {
            SessionEntry::Message(m) => Some(m.message.role().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "toolResult", "assistant"]);
    t.cleanup().await;
}

#[tokio::test]
async fn aborting_while_tools_run_ends_with_the_signal_reason_of_the_last_request() {
    // TS: the loop still requests one more response with the aborted signal; auth resolution
    // races that signal and rejects with its reason, so the turn ends with an error message
    // "This operation was aborted" (not provider-retry's "Request aborted").
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let started_tx = Arc::new(Mutex::new(Some(started_tx)));
    let setup: Setup = Arc::new(move |pi| {
        let started_tx = started_tx.clone();
        pi.register_tool(ToolDefinition::new(
            "block",
            "Block",
            "Blocks until aborted",
            json!({"type": "object", "properties": {}}),
            move |_id, _params, signal, _update, _ctx| {
                let started_tx = started_tx.clone();
                async move {
                    if let Some(tx) = started_tx.lock().take() {
                        let _ = tx.send(());
                    }
                    if let Some(signal) = signal {
                        signal.cancelled().await;
                    }
                    Ok(AgentToolResult::text("tool aborted", Some(json!({}))))
                }
            },
        ))
    });
    let t = create_runtime_for_test(Some(setup), None, true, false).await;
    t.faux.set_responses(vec![tool_use("block", json!({}))]);
    let session = t.runtime.session();
    let running = session.clone();
    let prompt = tokio::spawn(async move { running.prompt("start blocking tool", None).await });
    started_rx.await.unwrap();
    session.abort().await.unwrap();
    prompt.await.unwrap().unwrap();

    let assistants: Vec<_> = session
        .messages()
        .into_iter()
        .filter_map(|m| match m {
            AgentMessage::Assistant(a) => Some(a),
            _ => None,
        })
        .collect();
    assert_eq!(assistants.len(), 2);
    let last = assistants.last().unwrap();
    assert_eq!(last.stop_reason, pi_ai::types::StopReason::Error);
    assert_eq!(last.error_message.as_deref(), Some("This operation was aborted"));
    assert!(last.content.is_empty());
    t.cleanup().await;
}

#[tokio::test]
async fn preserves_an_existing_session_when_importing_a_file_with_the_same_name() {
    let t = create_runtime_for_test(None, None, true, false).await;
    let session_dir = t.runtime.session().session_manager().get_session_dir();
    let import_dir = format!("{}/import", t.temp_dir);
    let stored_path = format!("{session_dir}/collision.jsonl");
    let import_path = format!("{import_dir}/collision.jsonl");
    let header = |id: &str| {
        format!(
            "{}\n",
            json!({"type": "session", "version": 3, "id": id, "timestamp": chrono::Utc::now().to_rfc3339(), "cwd": t.temp_dir})
        )
    };
    let stored = header("stored");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::create_dir_all(&import_dir).unwrap();
    std::fs::write(&stored_path, &stored).unwrap();
    std::fs::write(&import_path, header("imported")).unwrap();

    t.runtime.import_from_jsonl(&import_path, None).await.unwrap();

    assert_eq!(std::fs::read_to_string(&stored_path).unwrap(), stored);
    let session_file = t.runtime.session().session_file().unwrap();
    assert_ne!(session_file, stored_path);
    assert!(std::fs::read_to_string(&session_file).unwrap().contains("\"id\":\"imported\""));
    t.cleanup().await;
}

#[tokio::test]
async fn emits_session_before_switch_and_session_start_for_new_and_resume_flows() {
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let t = create_runtime_for_test(
        Some(recorder(
            events.clone(),
            &["session_before_switch", "session_shutdown", "session_start"],
            Arc::new(never),
        )),
        None,
        true,
        false,
    )
    .await;

    assert_eq!(*events.lock(), vec![json!({"type": "session_start", "reason": "startup"})]);
    events.lock().clear();

    t.runtime.session().prompt("hello", None).await.unwrap();
    let original_file = t.runtime.session().session_file().unwrap();
    let original = t.runtime.session();

    let result = t.runtime.new_session(None).await.unwrap();
    assert!(!result.cancelled);
    bind(&t.runtime).await;
    assert!(!Arc::ptr_eq(&t.runtime.session(), &original));
    assert!(t.runtime.session().messages().is_empty());
    let second_file = t.runtime.session().session_file().unwrap();
    assert_eq!(
        *events.lock(),
        vec![
            json!({"type": "session_before_switch", "reason": "new"}),
            json!({"type": "session_shutdown", "reason": "new", "targetSessionFile": second_file}),
            json!({"type": "session_start", "reason": "new", "previousSessionFile": original_file}),
        ]
    );
    events.lock().clear();

    let result = t.runtime.switch_session(&original_file, None).await.unwrap();
    assert!(!result.cancelled);
    bind(&t.runtime).await;
    assert_eq!(
        *events.lock(),
        vec![
            json!({"type": "session_before_switch", "reason": "resume", "targetSessionFile": original_file}),
            json!({"type": "session_shutdown", "reason": "resume", "targetSessionFile": original_file}),
            json!({"type": "session_start", "reason": "resume", "previousSessionFile": second_file}),
        ]
    );
    t.cleanup().await;
}

#[tokio::test]
async fn honors_session_before_switch_cancellation_for_new_and_resume() {
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let cancel_reason = Arc::new(Mutex::new(None::<&'static str>));
    let cr = cancel_reason.clone();
    let t = create_runtime_for_test(
        Some(recorder(
            events.clone(),
            &["session_before_switch", "session_start"],
            Arc::new(move |event: &Value| {
                event["type"] == json!("session_before_switch")
                    && cr.lock().is_some_and(|r| event["reason"] == json!(r))
            }),
        )),
        None,
        true,
        false,
    )
    .await;
    t.runtime.session().prompt("hello", None).await.unwrap();
    let original_file = t.runtime.session().session_file();

    *cancel_reason.lock() = Some("new");
    let result = t.runtime.new_session(None).await.unwrap();
    assert!(result.cancelled);
    assert_eq!(t.runtime.session().session_file(), original_file);

    events.lock().clear();
    let other_dir = temp_dir("pi-runtime-other");
    let other = SessionManager::create(&other_dir, Some(&session_dir(&other_dir)), None).unwrap();
    other.append_message(user_blocks("other", now_ms())).unwrap();
    let other_file = other.get_session_file().unwrap();
    *cancel_reason.lock() = Some("resume");
    let result = t.runtime.switch_session(&other_file, None).await.unwrap();
    assert!(result.cancelled);
    assert_eq!(t.runtime.session().session_file(), original_file);
    let _ = std::fs::remove_dir_all(&other_dir);
    t.cleanup().await;
}

#[tokio::test]
async fn emits_session_before_fork_and_session_start_and_honors_cancellation() {
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let cancel_next = Arc::new(Mutex::new(false));
    let cn = cancel_next.clone();
    let t = create_runtime_for_test(
        Some(recorder(
            events.clone(),
            &["session_before_fork", "session_shutdown", "session_start"],
            Arc::new(move |event: &Value| {
                event["type"] == json!("session_before_fork") && std::mem::replace(&mut *cn.lock(), false)
            }),
        )),
        None,
        true,
        false,
    )
    .await;
    events.lock().clear();
    t.runtime.session().prompt("hello", None).await.unwrap();
    let user_message = t.runtime.session().get_user_messages_for_forking()[0].clone();
    let previous_file = t.runtime.session().session_file().unwrap();

    let result = t.runtime.fork(&user_message.entry_id, None).await.unwrap();
    assert!(!result.cancelled);
    assert_eq!(result.selected_text.as_deref(), Some("hello"));
    bind(&t.runtime).await;
    let new_file = t.runtime.session().session_file().unwrap();
    assert_eq!(
        *events.lock(),
        vec![
            json!({"type": "session_before_fork", "entryId": user_message.entry_id, "position": "before"}),
            json!({"type": "session_shutdown", "reason": "fork", "targetSessionFile": new_file}),
            json!({"type": "session_start", "reason": "fork", "previousSessionFile": previous_file}),
        ]
    );
    let file_name = std::path::Path::new(&new_file).file_stem().unwrap().to_string_lossy().into_owned();
    assert!(file_name.ends_with(&format!("_{}", t.runtime.session().session_id())));

    events.lock().clear();
    *cancel_next.lock() = true;
    let result = t.runtime.fork(&user_message.entry_id, None).await.unwrap();
    assert_eq!(serde_json::to_value(&result).unwrap(), json!({"cancelled": true}));
    assert_eq!(
        *events.lock(),
        vec![json!({"type": "session_before_fork", "entryId": user_message.entry_id, "position": "before"})]
    );

    events.lock().clear();
    *cancel_next.lock() = true;
    let result = t
        .runtime
        .fork(
            "missing-entry",
            Some(&AgentSessionRuntimeForkOptions {
                position: Some(AgentSessionRuntimeForkOptionsPosition::At),
                with_session: None,
            }),
        )
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(&result).unwrap(), json!({"cancelled": true}));
    assert_eq!(
        *events.lock(),
        vec![json!({"type": "session_before_fork", "entryId": "missing-entry", "position": "at"})]
    );
    t.cleanup().await;
}

fn fork_at() -> AgentSessionRuntimeForkOptions {
    AgentSessionRuntimeForkOptions {
        position: Some(AgentSessionRuntimeForkOptionsPosition::At),
        with_session: None,
    }
}

#[tokio::test]
async fn reports_why_an_unflushed_session_cannot_be_forked() {
    let t = create_runtime_for_test(None, None, true, false).await;
    let session_file = t.runtime.session().session_file().expect("session file");
    let leaf_id = t.runtime.session().session_manager().get_leaf_id().expect("leaf id");
    assert!(!std::path::Path::new(&session_file).exists());

    let error = t.runtime.fork(&leaf_id, Some(&fork_at())).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("This session has not been saved yet. Send a message before cloning or forking it."),
        "{error}"
    );
    t.cleanup().await;
}

#[tokio::test]
async fn duplicates_the_current_active_branch_when_forking_at_the_current_position() {
    let t = create_runtime_for_test(None, None, true, false).await;
    t.runtime.session().prompt("hello", None).await.unwrap();
    t.runtime.session().prompt("again", None).await.unwrap();
    let before = roles_and_user_texts(&t.runtime.session().messages());
    let previous_file = t.runtime.session().session_file();
    let leaf_id = t.runtime.session().session_manager().get_leaf_id().unwrap();

    let result = t.runtime.fork(&leaf_id, Some(&fork_at())).await.unwrap();
    assert_eq!(serde_json::to_value(&result).unwrap(), json!({"cancelled": false}));
    assert_ne!(t.runtime.session().session_file(), previous_file);
    assert_eq!(roles_and_user_texts(&t.runtime.session().messages()), before);
    t.cleanup().await;
}

#[tokio::test]
async fn duplicates_the_current_active_branch_in_memory_when_forking_at_the_current_position() {
    let t = create_runtime_for_test(None, Some(temp_dir("pi-runtime-suite-in-memory")), true, true).await;
    t.runtime.session().prompt("hello", None).await.unwrap();
    t.runtime.session().prompt("again", None).await.unwrap();
    let before = roles_and_user_texts(&t.runtime.session().messages());
    let leaf_id = t.runtime.session().session_manager().get_leaf_id().unwrap();
    assert!(t.runtime.session().session_file().is_none());

    let result = t.runtime.fork(&leaf_id, Some(&fork_at())).await.unwrap();
    assert_eq!(serde_json::to_value(&result).unwrap(), json!({"cancelled": false}));
    assert!(t.runtime.session().session_file().is_none());
    assert_eq!(roles_and_user_texts(&t.runtime.session().messages()), before);
    t.cleanup().await;
}

#[tokio::test]
async fn throws_when_forking_with_an_invalid_entry_id() {
    let t = create_runtime_for_test(None, None, true, false).await;
    let error = t.runtime.fork("missing-entry", None).await.unwrap_err();
    assert!(error.to_string().contains("Invalid entry ID for forking"), "{error}");
    t.cleanup().await;
}

/// A second runtime on `cwd` sharing the faux provider (TS `createOtherRuntime`).
async fn other_runtime(t: &RuntimeTest, cwd: &str) -> Arc<AgentSessionRuntime> {
    let model_runtime = model_runtime_for(&t.faux).await;
    let factory = factory_for(&t.faux, &t.temp_dir, model_runtime, None, None);
    create_agent_session_runtime(
        &factory,
        &CreateAgentSessionRuntimeOptions {
            cwd: cwd.to_string(),
            agent_dir: t.temp_dir.clone(),
            session_manager: SessionManager::create(cwd, Some(&session_dir(cwd)), None).unwrap(),
            session_start_event: None,
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn updates_the_runtime_session_cwd_on_cross_cwd_session_replacement() {
    let first_dir = temp_dir("pi-runtime-cwd-a");
    let second_dir = temp_dir("pi-runtime-cwd-b");
    let t = create_runtime_for_test(None, Some(first_dir.clone()), true, false).await;
    let other = other_runtime(&t, &second_dir).await;
    other.session().prompt("other", None).await.unwrap();
    let other_file = other.session().session_file().unwrap();

    t.runtime.switch_session(&other_file, None).await.unwrap();

    let canonical = |p: &str| std::fs::canonicalize(p).unwrap();
    assert_eq!(canonical(&t.runtime.session().session_manager().get_cwd()), canonical(&second_dir));
    assert_eq!(canonical(&t.runtime.cwd()), canonical(&second_dir));
    let _ = other.dispose().await;
    let _ = std::fs::remove_dir_all(&second_dir);
    t.cleanup().await;
}

#[tokio::test]
async fn restores_model_and_thinking_state_from_the_destination_session() {
    let t = create_runtime_for_test(None, None, false, false).await;
    let other_dir = format!("{}/other", t.temp_dir);
    std::fs::create_dir_all(&other_dir).unwrap();
    let other = other_runtime(&t, &other_dir).await;
    other
        .session()
        .set_model(t.faux.get_model_by_id("faux-2").unwrap(), None)
        .await
        .unwrap();
    other.session().set_thinking_level(ThinkingLevel::Off, None);
    other.session().prompt("hello", None).await.unwrap();
    let target = other.session().session_file().unwrap();

    t.runtime.switch_session(&target, None).await.unwrap();

    assert_eq!(t.runtime.session().model().unwrap().id, "faux-2");
    assert_eq!(t.runtime.session().thinking_level(), ThinkingLevel::Off);
    let _ = other.dispose().await;
    t.cleanup().await;
}

// Port of the case of `test/agent-session-runtime-events.test.ts` the suite above does not cover.
#[tokio::test]
async fn runs_before_session_invalidate_after_session_shutdown_and_before_rebind_session() {
    let phases = Arc::new(Mutex::new(Vec::<String>::new()));
    let p = phases.clone();
    let setup: Setup = Arc::new(move |pi| {
        let p = p.clone();
        pi.on(
            "session_shutdown",
            Arc::new(move |_event: ExtensionEvent, _ctx| {
                let p = p.clone();
                Box::pin(async move {
                    p.lock().push("session_shutdown".into());
                    Ok(None)
                })
            }),
        )?;
        Ok(())
    });
    let t = create_runtime_for_test(Some(setup), None, true, false).await;
    let old_session = t.runtime.session();
    let (p, old) = (phases.clone(), old_session.clone());
    let cwd_at_invalidate = Arc::new(Mutex::new(None::<(String, String)>));
    let c = cwd_at_invalidate.clone();
    t.runtime.set_before_session_invalidate(Some(Arc::new(move || {
        p.lock().push("beforeSessionInvalidate".into());
        *c.lock() = Some((
            old.extension_runner().create_context().cwd().unwrap(),
            old.session_manager().get_cwd(),
        ));
    })));
    let p = phases.clone();
    t.runtime.set_rebind_session(Some(Arc::new(move |_session| {
        let p = p.clone();
        Box::pin(async move {
            p.lock().push("rebindSession".into());
            Ok(())
        })
    })));

    t.runtime.new_session(None).await.unwrap();

    assert_eq!(*phases.lock(), vec!["session_shutdown", "beforeSessionInvalidate", "rebindSession"]);
    let (ctx_cwd, manager_cwd) = cwd_at_invalidate.lock().clone().unwrap();
    assert_eq!(ctx_cwd, manager_cwd);
    let error = old_session.extension_runner().create_context().cwd().unwrap_err();
    assert_eq!(
        error.to_string(),
        "This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload(). For newSession, fork, and switchSession, move post-replacement work into withSession and use the ctx passed to withSession. For reload, do not use the old ctx after await ctx.reload()."
    );
    t.runtime.set_before_session_invalidate(None);
    t.runtime.set_rebind_session(None);
    t.cleanup().await;
}

// ---------------------------------------------------------------------------
// Regression #2860: replaced session callbacks.
// ---------------------------------------------------------------------------

mod replaced_session_context {
    use super::*;
    use pi_coding_agent::core::agent_session::AgentSessionNavigateTreeOptions;
    use pi_coding_agent::core::agent_session_runtime::{
        AgentSessionRuntimeNewSessionOptions, AgentSessionRuntimeSwitchSessionOptions,
    };
    use pi_coding_agent::core::extensions::types::{
        CommandOptions, ExtensionCommandContext, ExtensionCommandContextActions, ForkOptions, ForkPosition,
        NavigateTreeOptions, NewSessionOptions, SendUserMessageOptions, SessionReplaceOutcome,
        SessionShutdownEvent, SessionStartEvent, SwitchSessionOptions,
    };
    use pi_ai::types::UserContent;
    use std::sync::Weak;

    /// The TS test's `commandContextActions` (runtime host for replacement, current session otherwise).
    struct RuntimeActions(Weak<AgentSessionRuntime>);

    impl RuntimeActions {
        fn runtime(&self) -> anyhow::Result<Arc<AgentSessionRuntime>> {
            self.0.upgrade().ok_or_else(|| anyhow::anyhow!("runtime disposed"))
        }
    }

    #[async_trait::async_trait]
    impl ExtensionCommandContextActions for RuntimeActions {
        async fn wait_for_idle(&self) -> anyhow::Result<()> {
            self.runtime()?.session().agent().wait_for_idle().await;
            Ok(())
        }
        async fn new_session(&self, options: NewSessionOptions) -> anyhow::Result<SessionReplaceOutcome> {
            let result = self
                .runtime()?
                .new_session(Some(&AgentSessionRuntimeNewSessionOptions {
                    parent_session: options.parent_session,
                    setup: options.setup,
                    with_session: options.with_session,
                }))
                .await?;
            Ok(SessionReplaceOutcome { cancelled: result.cancelled })
        }
        async fn fork(&self, entry_id: &str, options: ForkOptions) -> anyhow::Result<SessionReplaceOutcome> {
            let result = self
                .runtime()?
                .fork(
                    entry_id,
                    Some(&AgentSessionRuntimeForkOptions {
                        position: options.position.map(|p| match p {
                            ForkPosition::Before => AgentSessionRuntimeForkOptionsPosition::Before,
                            ForkPosition::At => AgentSessionRuntimeForkOptionsPosition::At,
                        }),
                        with_session: options.with_session,
                    }),
                )
                .await?;
            Ok(SessionReplaceOutcome { cancelled: result.cancelled })
        }
        async fn navigate_tree(
            &self,
            target_id: &str,
            options: NavigateTreeOptions,
        ) -> anyhow::Result<SessionReplaceOutcome> {
            let result = self
                .runtime()?
                .session()
                .navigate_tree(
                    target_id,
                    Some(&AgentSessionNavigateTreeOptions {
                        summarize: options.summarize,
                        custom_instructions: options.custom_instructions,
                        replace_instructions: options.replace_instructions,
                        label: options.label,
                    }),
                )
                .await?;
            Ok(SessionReplaceOutcome { cancelled: result.cancelled })
        }
        async fn switch_session(
            &self,
            session_path: &str,
            options: SwitchSessionOptions,
        ) -> anyhow::Result<SessionReplaceOutcome> {
            let result = self
                .runtime()?
                .switch_session(
                    session_path,
                    Some(&AgentSessionRuntimeSwitchSessionOptions {
                        cwd_override: None,
                        with_session: options.with_session,
                        project_trust_context_factory: None,
                    }),
                )
                .await?;
            Ok(SessionReplaceOutcome { cancelled: result.cancelled })
        }
        async fn reload(&self) -> anyhow::Result<()> {
            self.runtime()?.session().reload(None).await
        }
    }

    async fn rebind(runtime: &Arc<AgentSessionRuntime>) -> anyhow::Result<()> {
        runtime
            .session()
            .bind_extensions(&ExtensionBindings {
                command_context_actions: Some(Arc::new(RuntimeActions(Arc::downgrade(runtime)))),
                ..Default::default()
            })
            .await
    }

    async fn create(setup: Setup, responses: &[&str]) -> RuntimeTest {
        let registry_guard = RegistryGuard::Shared(shared_api_registry().await);
        let temp_dir = temp_dir("pi-2860");
        let faux = register_faux();
        faux.set_responses(responses.iter().map(|r| text(r)).collect());
        let model_runtime = model_runtime_for(&faux).await;
        let factory = factory_for(&faux, &temp_dir, model_runtime, Some(setup), Some(faux.get_model()));
        let runtime = create_agent_session_runtime(
            &factory,
            &CreateAgentSessionRuntimeOptions {
                cwd: temp_dir.clone(),
                agent_dir: temp_dir.clone(),
                session_manager: SessionManager::create(&temp_dir, Some(&session_dir(&temp_dir)), None).unwrap(),
                session_start_event: None,
            },
        )
        .await
        .unwrap();
        let weak = Arc::downgrade(&runtime);
        runtime.set_rebind_session(Some(Arc::new(move |_session| {
            let weak = weak.clone();
            Box::pin(async move {
                match weak.upgrade() {
                    Some(runtime) => rebind(&runtime).await,
                    None => Ok(()),
                }
            })
        })));
        rebind(&runtime).await.unwrap();
        RuntimeTest {
            runtime,
            faux,
            temp_dir,
            _registry_guard: registry_guard,
        }
    }

    fn command(
        pi: &ExtensionAPIRef,
        name: &str,
        handler: impl Fn(ExtensionCommandContext) -> futures::future::BoxFuture<'static, anyhow::Result<()>>
        + Send
        + Sync
        + 'static,
    ) -> anyhow::Result<()> {
        let handler = Arc::new(handler);
        pi.register_command(
            name,
            CommandOptions {
                description: Some(name.into()),
                get_argument_completions: None,
                handler: Arc::new(move |_args, ctx| handler(ctx)),
            },
        )
    }

    fn texts(runtime: &AgentSessionRuntime) -> Vec<String> {
        runtime
            .session()
            .messages()
            .iter()
            .filter(|m| m.role() != "system")
            .map(|m| format!("{}:{}", m.role(), get_message_text(m)))
            .collect()
    }

    fn user_text(text: &str) -> UserContent {
        UserContent::Text(text.into())
    }

    #[tokio::test]
    async fn rebinds_before_with_session_targets_the_replacement_session_and_invalidates_stale_pi_and_ctx() {
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        #[derive(Default)]
        struct Observed {
            old_session_file: Option<String>,
            replacement_session_file: Option<String>,
            stale_ctx_throws: bool,
            stale_pi_throws: bool,
        }
        let observed = Arc::new(Mutex::new(Observed::default()));
        let instance_id = Arc::new(Mutex::new(0));
        let (ev, ob) = (events.clone(), observed.clone());
        let setup: Setup = Arc::new(move |pi| {
            let current = {
                let mut id = instance_id.lock();
                *id += 1;
                *id
            };
            let e = ev.clone();
            on_event::<SessionStartEvent, _, _>(&*pi, move |_event, _ctx| {
                e.lock().push(format!("start:{current}"));
                async { Ok(None) }
            })?;
            let e = ev.clone();
            on_event::<SessionShutdownEvent, _, _>(&*pi, move |_event, _ctx| {
                e.lock().push(format!("shutdown:{current}"));
                async { Ok(None) }
            })?;
            let (e, ob, old_pi) = (ev.clone(), ob.clone(), pi.clone());
            command(&pi, "repro", move |ctx| {
                let (e, ob, old_pi) = (e.clone(), ob.clone(), old_pi.clone());
                Box::pin(async move {
                    let old_ctx = ctx.clone();
                    let old_session_file = ctx.session_manager()?.get_session_file();
                    ob.lock().old_session_file = old_session_file.clone();
                    let with_session: pi_coding_agent::core::extensions::types::WithSessionFn =
                        Arc::new(move |replaced| {
                            let (e, ob, old_ctx, old_pi) = (e.clone(), ob.clone(), old_ctx.clone(), old_pi.clone());
                            Box::pin(async move {
                                e.lock().push(format!("with:{current}"));
                                {
                                    let mut ob = ob.lock();
                                    ob.replacement_session_file = replaced.session_manager()?.get_session_file();
                                    ob.stale_ctx_throws = old_ctx.session_manager().is_err();
                                    ob.stale_pi_throws = old_pi
                                        .send_user_message(user_text("stale message"), SendUserMessageOptions::default())
                                        .is_err();
                                }
                                replaced
                                    .send_user_message(
                                        user_text("Hello from the new session!"),
                                        SendUserMessageOptions::default(),
                                    )
                                    .await
                            })
                        });
                    ctx.new_session(NewSessionOptions {
                        parent_session: old_session_file,
                        setup: None,
                        with_session: Some(with_session),
                    })
                    .await?;
                    Ok(())
                })
            })
        });
        let t = create(setup, &["hello reply"]).await;

        assert_eq!(*events.lock(), vec!["start:1"]);

        t.runtime.session().prompt("/repro", None).await.unwrap();

        assert_eq!(*events.lock(), vec!["start:1", "shutdown:1", "start:2", "with:1"]);
        {
            let ob = observed.lock();
            assert!(ob.replacement_session_file.is_some());
            assert_ne!(ob.replacement_session_file, ob.old_session_file);
            assert!(ob.stale_ctx_throws);
            assert!(ob.stale_pi_throws);
        }
        assert_eq!(
            texts(&t.runtime),
            vec!["user:Hello from the new session!", "assistant:hello reply"]
        );
        t.cleanup().await;
    }

    #[tokio::test]
    async fn supports_with_session_for_fork() {
        let setup: Setup = Arc::new(|pi| {
            command(&pi, "fork-it", |ctx| {
                Box::pin(async move {
                    let leaf_id = ctx
                        .session_manager()?
                        .get_leaf_id()
                        .ok_or_else(|| anyhow::anyhow!("Missing leaf id"))?;
                    ctx.fork(
                        &leaf_id,
                        ForkOptions {
                            position: Some(ForkPosition::At),
                            with_session: Some(Arc::new(|replaced| {
                                Box::pin(async move {
                                    replaced
                                        .send_user_message(
                                            user_text("fork callback message"),
                                            SendUserMessageOptions::default(),
                                        )
                                        .await
                                })
                            })),
                        },
                    )
                    .await?;
                    Ok(())
                })
            })
        });
        let t = create(setup, &["seed reply", "fork reply"]).await;

        t.runtime.session().prompt("seed", None).await.unwrap();
        t.runtime.session().prompt("/fork-it", None).await.unwrap();

        assert_eq!(
            texts(&t.runtime),
            vec!["user:seed", "assistant:seed reply", "user:fork callback message", "assistant:fork reply"]
        );
        t.cleanup().await;
    }

    #[tokio::test]
    async fn supports_with_session_for_switch_session() {
        let target_session_path = Arc::new(Mutex::new(String::new()));
        let target = target_session_path.clone();
        let setup: Setup = Arc::new(move |pi| {
            let target = target.clone();
            command(&pi, "switch-it", move |ctx| {
                let target = target.lock().clone();
                Box::pin(async move {
                    ctx.switch_session(
                        &target,
                        SwitchSessionOptions {
                            with_session: Some(Arc::new(|replaced| {
                                Box::pin(async move {
                                    replaced
                                        .send_user_message(
                                            user_text("switch callback message"),
                                            SendUserMessageOptions::default(),
                                        )
                                        .await
                                })
                            })),
                        },
                    )
                    .await?;
                    Ok(())
                })
            })
        });
        let t = create(setup, &["root reply", "target reply", "switch reply"]).await;

        t.runtime.session().prompt("root", None).await.unwrap();
        let original_session_path = t.runtime.session().session_file().unwrap();
        let result = t.runtime.new_session(None).await.unwrap();
        assert!(!result.cancelled);
        t.runtime.session().prompt("target", None).await.unwrap();
        *target_session_path.lock() = t.runtime.session().session_file().unwrap();
        t.runtime.switch_session(&original_session_path, None).await.unwrap();

        t.runtime.session().prompt("/switch-it", None).await.unwrap();

        assert_eq!(t.runtime.session().session_file(), Some(target_session_path.lock().clone()));
        assert_eq!(
            texts(&t.runtime),
            vec!["user:target", "assistant:target reply", "user:switch callback message", "assistant:switch reply"]
        );
        t.cleanup().await;
    }
}

// Regression #2753: reload applies updated top-level resource settings.
#[tokio::test]
async fn applies_updated_top_level_prompt_settings_on_reload_after_startup() {
    let temp_dir = temp_dir("pi-2753");
    let agent_dir = format!("{temp_dir}/agent");
    let prompts_dir = format!("{agent_dir}/prompts");
    std::fs::create_dir_all(&prompts_dir).unwrap();
    std::fs::write(format!("{prompts_dir}/test.md"), "Echo test prompt\n").unwrap();

    // `reload()` resets the global api registry.
    let registry_guard = RegistryGuard::Exclusive(exclusive_api_registry().await);
    let faux = register_faux();
    let model_runtime = model_runtime_for(&faux).await;
    let provider = faux.get_model().provider.clone();
    let base_url = faux.get_model().base_url.clone();
    let api = faux.api().to_string();
    let models = provider_models(&faux);
    let model = faux.get_model();
    let ad = agent_dir.clone();
    let factory: CreateAgentSessionRuntimeFactory = Arc::new(move |options| {
        let (provider, base_url, api, models) = (provider.clone(), base_url.clone(), api.clone(), models.clone());
        let (agent_dir, model_runtime, model) = (ad.clone(), model_runtime.clone(), model.clone());
        Box::pin(async move {
            let extension: ExtensionFactory = Arc::new(move |pi: ExtensionAPIRef| {
                let (provider, base_url, api, models) = (provider.clone(), base_url.clone(), api.clone(), models.clone());
                Box::pin(async move {
                    pi.register_provider(ProviderRegistration::Config {
                        name: provider,
                        config: ProviderConfig {
                            base_url: Some(base_url),
                            api_key: Some("faux-key".into()),
                            api: Some(api),
                            models: Some(models),
                            ..Default::default()
                        },
                    })?;
                    Ok(())
                })
            });
            let services = create_agent_session_services(&CreateAgentSessionServicesOptions {
                cwd: options.cwd.clone(),
                agent_dir: Some(agent_dir),
                settings_manager: None,
                model_runtime: Some(model_runtime),
                model_runtime_signal: None,
                extension_flag_values: None,
                resource_loader_options: Some(DefaultResourceLoaderOptions {
                    extension_factories: Some(vec![InlineExtension::Factory(extension)]),
                    no_skills: Some(true),
                    no_themes: Some(true),
                    ..Default::default()
                }),
                resource_loader_reload_options: None,
            })
            .await?;
            let created = create_agent_session_from_services(&CreateAgentSessionFromServicesOptions {
                services: services.clone(),
                session_manager: options.session_manager.clone(),
                session_start_event: options.session_start_event.clone(),
                model: Some(model),
                thinking_level: None,
                scoped_models: None,
                tools: None,
                exclude_tools: None,
                no_tools: None,
                custom_tools: None,
            })
            .await?;
            Ok(CreateAgentSessionRuntimeResult {
                session: created.session,
                extensions_result: created.extensions_result,
                model_fallback_message: created.model_fallback_message,
                diagnostics: services.diagnostics.clone(),
                services,
            })
        })
    });
    let runtime = create_agent_session_runtime(
        &factory,
        &CreateAgentSessionRuntimeOptions {
            cwd: temp_dir.clone(),
            agent_dir: agent_dir.clone(),
            session_manager: SessionManager::create(&temp_dir, Some(&session_dir(&temp_dir)), None).unwrap(),
            session_start_event: None,
        },
    )
    .await
    .unwrap();
    let t = RuntimeTest {
        runtime,
        faux,
        temp_dir,
        _registry_guard: registry_guard,
    };
    let names = |runtime: &AgentSessionRuntime| -> Vec<String> {
        runtime.session().prompt_templates().into_iter().map(|p| p.name).collect()
    };

    assert!(names(&t.runtime).contains(&"test".to_string()));

    std::fs::write(
        format!("{agent_dir}/settings.json"),
        format!("{}\n", serde_json::to_string_pretty(&json!({"prompts": ["-prompts/test.md"]})).unwrap()),
    )
    .unwrap();

    t.runtime.session().reload(None).await.unwrap();

    assert_eq!(
        t.runtime.services().settings_manager.get_global_settings().prompts,
        Some(vec!["-prompts/test.md".to_string()])
    );
    assert!(!names(&t.runtime).contains(&"test".to_string()));
    t.cleanup().await;
}

// ---------------------------------------------------------------------------
// `test/agent-session-branching.test.ts` (an API-key e2e suite in TS) on the faux provider.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn branching_allows_forking_from_a_single_message() {
    let t = create_runtime_for_test(None, None, true, false).await;
    let session = t.runtime.session();
    session.prompt("Say hello", None).await.unwrap();
    session.agent().wait_for_idle().await;

    let user_messages = session.get_user_messages_for_forking();
    assert_eq!(user_messages.len(), 1);
    assert_eq!(user_messages[0].text, "Say hello");

    let result = t.runtime.fork(&user_messages[0].entry_id, None).await.unwrap();
    assert!(!result.cancelled);
    let session = t.runtime.session();
    assert_eq!(result.selected_text.as_deref(), Some("Say hello"));

    // TS expects no messages here, but that API-key suite predates transcript system messages: the
    // system message recorded before the first user message stays on the fork (as in #8724).
    let roles: Vec<String> = session.messages().iter().map(|m| m.role().to_string()).collect();
    assert_eq!(roles, vec!["system"]);
    let file = session.session_file().expect("session file");
    assert!(!std::path::Path::new(&file).exists());
    t.cleanup().await;
}

#[tokio::test]
async fn branching_supports_in_memory_forking_in_no_session_mode() {
    let t = create_runtime_for_test(None, None, true, true).await;
    let session = t.runtime.session();
    assert!(session.session_file().is_none());

    session.prompt("Say hi", None).await.unwrap();
    session.agent().wait_for_idle().await;

    let user_messages = session.get_user_messages_for_forking();
    assert_eq!(user_messages.len(), 1);
    assert!(!session.messages().is_empty());

    let result = t.runtime.fork(&user_messages[0].entry_id, None).await.unwrap();
    assert!(!result.cancelled);
    let session = t.runtime.session();
    assert_eq!(result.selected_text.as_deref(), Some("Say hi"));

    // TS expects no messages here, but that API-key suite predates transcript system messages: the
    // system message recorded before the first user message stays on the fork (as in #8724).
    let roles: Vec<String> = session.messages().iter().map(|m| m.role().to_string()).collect();
    assert_eq!(roles, vec!["system"]);
    assert!(session.session_file().is_none());
    t.cleanup().await;
}

#[tokio::test]
async fn branching_forks_from_the_middle_of_a_conversation() {
    let t = create_runtime_for_test(None, None, true, false).await;
    let session = t.runtime.session();
    for prompt in ["Say one", "Say two", "Say three"] {
        session.prompt(prompt, None).await.unwrap();
        session.agent().wait_for_idle().await;
    }

    let user_messages = session.get_user_messages_for_forking();
    assert_eq!(user_messages.len(), 3);

    let result = t.runtime.fork(&user_messages[1].entry_id, None).await.unwrap();
    assert!(!result.cancelled);
    assert_eq!(result.selected_text.as_deref(), Some("Say two"));

    let roles: Vec<String> = t
        .runtime
        .session()
        .messages()
        .iter()
        .filter(|m| m.role() != "system")
        .map(|m| m.role().to_string())
        .collect();
    assert_eq!(roles, vec!["user", "assistant"]);
    t.cleanup().await;
}
