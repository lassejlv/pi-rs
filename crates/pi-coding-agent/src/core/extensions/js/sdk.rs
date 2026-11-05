//! SDK classes of `@earendil-works/pi-coding-agent` for JavaScript extensions, backed by the Rust
//! implementations: `SettingsManager` (constructed with its static factories).

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use super::Bridge;
use super::api::{arg, str_arg};
use crate::compat::WheelScrollLines;
use crate::core::settings_manager::*;
use crate::modes::interactive::components::tree_selector::FilterMode;
use pi_agent::types::ThinkingLevel;
use pi_tui::components::scroll_view::ScrollViewScrollbar;

type JsonMap = serde_json::Map<String, Value>;

pub(crate) fn dispatch(bridge: &Arc<Bridge>, op: &str, payload: Value) -> anyhow::Result<Value> {
    match op {
        "settings" => settings_call(bridge, &payload),
        "settings.static" => settings_static(bridge, &payload),
        "rt" => runtime_call(bridge, &payload),
        "rl" => loader_call(bridge, &payload),
        "as" => session_call(bridge, &payload),
        "trust" | "pm" | "sdk.resolveCliModel" | "sdk.createExtensionRuntime" | "sdk.servicesPaths" | "sdk.extensionTool" => {
            misc_call(bridge, op, &payload)
        }
        _ => Err(anyhow::anyhow!("unknown host call {op}")),
    }
}

pub(crate) async fn dispatch_async(bridge: Arc<Bridge>, op: &str, payload: Value) -> anyhow::Result<Value> {
    match op {
        "settings.async" => settings_async(&bridge, &payload).await,
        "rt.async" => runtime_async(&bridge, &payload).await,
        "rt.stream" => runtime_stream(&bridge, &payload),
        "rl.async" => {
            let loader = loader_of(&bridge, &payload)?;
            let options = reload_options(&bridge, &payload["resolveProjectTrust"]);
            loader.reload(options.as_ref()).await?;
            Ok(Value::Null)
        }
        "as.async" => session_async(&bridge, &payload).await,
        "builtin.ext.run" => run_builtin_extension(&bridge, &payload).await,
        "pm.async" | "sdk.resolveModelScope" | "sdk.discoverAndLoadExtensions" | "sdk.finishServices" => {
            misc_async(&bridge, op, &payload).await
        }
        _ => Err(anyhow::anyhow!("unknown async host call {op}")),
    }
}

fn settings_of(bridge: &Bridge, payload: &Value) -> anyhow::Result<Arc<SettingsManager>> {
    bridge
        .settings_managers
        .get(payload["id"].as_u64().unwrap_or_default())
        .ok_or_else(|| anyhow::anyhow!("This settings manager is no longer available"))
}

/// `SettingsManager.create()` / `inMemory()` / `fromStorage()`: the new manager is owned by its JS
/// object (released when it is collected).
fn settings_static(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let method = str_arg(payload, "m");
    let options: Option<SettingsManagerCreateOptions> = super::api::opt(&payload["options"])?;
    let manager = match method.as_str() {
        "create" => SettingsManager::create(
            payload["cwd"].as_str().unwrap_or(&bridge.cwd),
            payload["agentDir"].as_str(),
            options.as_ref(),
        ),
        "inMemory" => SettingsManager::in_memory_json(
            payload["settings"].as_object().cloned().unwrap_or_default(),
            options.as_ref(),
        ),
        "fromStorage" => {
            let storage = bridge
                .handle(&payload["storage"])
                .ok_or_else(|| anyhow::anyhow!("SettingsManager.fromStorage() expects a storage object"))?;
            SettingsManager::from_storage(Arc::new(JsSettingsStorage { bridge: Arc::downgrade(bridge), storage }), options.as_ref())
        }
        "release" => {
            bridge.settings_managers.remove(payload["id"].as_u64().unwrap_or_default());
            return Ok(Value::Null);
        }
        _ => return Err(anyhow::anyhow!("SettingsManager.{method} is not a function")),
    };
    Ok(json!(bridge.settings_managers.insert(manager)))
}

/// A JavaScript `SettingsStorage` (`{ withLock(scope, fn) }`).
struct JsSettingsStorage {
    bridge: std::sync::Weak<Bridge>,
    storage: pi_jsrt::JsHandle,
}

/// The lock callback for the duration of one `withLock` call into JavaScript.
struct LockFn(*mut (dyn FnMut(Option<String>) -> anyhow::Result<Option<String>> + 'static));
// SAFETY: the pointer is only dereferenced while `with_lock` is running (it is cleared before
// `with_lock` returns), during which the callback it points to is alive and not used elsewhere.
unsafe impl Send for LockFn {}
unsafe impl Sync for LockFn {}

impl SettingsStorage for JsSettingsStorage {
    fn with_lock(&self, scope: SettingsScope, f: SettingsLockFn<'_>) -> anyhow::Result<()> {
        let bridge = self.bridge.upgrade().ok_or_else(|| anyhow::anyhow!("extension runtime stopped"))?;
        let ptr: *mut (dyn FnMut(Option<String>) -> anyhow::Result<Option<String>> + '_) = f;
        // SAFETY: lifetime erasure only; see `LockFn`.
        let ptr: *mut (dyn FnMut(Option<String>) -> anyhow::Result<Option<String>> + 'static) =
            unsafe { std::mem::transmute(ptr) };
        let slot = Arc::new(Mutex::new(Some(LockFn(ptr))));
        let callback_slot = slot.clone();
        let id = bridge.register_callback(Arc::new(move |args: Vec<Value>| {
            let guard = callback_slot.lock();
            let Some(f) = guard.as_ref() else {
                anyhow::bail!("withLock callback called after withLock returned");
            };
            let current = args.first().and_then(Value::as_str).map(str::to_string);
            // SAFETY: see `LockFn`.
            let next = unsafe { (*f.0)(current) }?;
            Ok(json!(next))
        }));
        let scope = serde_json::to_value(scope)?;
        let result = bridge.glue_call_blocking("invokeWithLock", vec![self.storage.to_value(), scope, json!(id)]);
        *slot.lock() = None;
        bridge.callbacks.remove(id);
        result.map(|_| ())
    }
}

fn settings_error_to_js(error: &SettingsError) -> Value {
    let mut out = json!({ "scope": serde_json::to_value(error.scope).unwrap_or(Value::Null) });
    if let Some(path) = &error.path {
        out["path"] = json!(path);
    }
    out["error"] = json!({ "message": crate::core::extensions::runner::error_parts(&error.error).0 });
    out
}

fn capability_overrides_to_js(overrides: &pi_tui::terminal_image::CapabilityOverrides) -> Value {
    let mut out = serde_json::Map::new();
    match overrides.images {
        Some(Some(pi_tui::terminal_image::ImageProtocol::Kitty)) => {
            out.insert("images".into(), json!("kitty"));
        }
        Some(Some(pi_tui::terminal_image::ImageProtocol::Iterm2)) => {
            out.insert("images".into(), json!("iterm2"));
        }
        Some(None) => {
            out.insert("images".into(), Value::Null);
        }
        None => {}
    }
    if let Some(v) = overrides.true_color {
        out.insert("trueColor".into(), json!(v));
    }
    if let Some(v) = overrides.hyperlinks {
        out.insert("hyperlinks".into(), json!(v));
    }
    Value::Object(out)
}

/// Methods of a `SettingsManager` instance (TS names, Rust implementations).
fn settings_call(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let sm = settings_of(bridge, payload)?;
    let method = str_arg(payload, "m");
    let s = |i: usize| arg(payload, i).as_str().unwrap_or_default().to_string();
    let os = |i: usize| arg(payload, i).as_str().map(str::to_string);
    let b = |i: usize| arg(payload, i).as_bool().unwrap_or(false);
    let n = |i: usize| arg(payload, i).as_f64().unwrap_or_default() as i64;
    fn de<T: serde::de::DeserializeOwned>(payload: &Value, i: usize) -> anyhow::Result<T> {
        serde_json::from_value(arg(payload, i)).map_err(|e| anyhow::anyhow!("Invalid argument: {e}"))
    }
    Ok(match method.as_str() {
        "getModelThinkingLevel" => serde_json::to_value(sm.get_model_thinking_level(&s(0), &s(1)))?,
        "getCompactionReserveTokens" => serde_json::to_value(sm.get_compaction_reserve_tokens(de::<Option<SettingsManagerGetCompactionSettingsModel>>(payload, 0)?.as_ref())?)?,
        "getCompactionKeepRecentTokens" => serde_json::to_value(sm.get_compaction_keep_recent_tokens(de::<Option<SettingsManagerGetCompactionSettingsModel>>(payload, 0)?.as_ref())?)?,
        "getCompactionSettings" => serde_json::to_value(sm.get_compaction_settings(de::<Option<SettingsManagerGetCompactionSettingsModel>>(payload, 0)?.as_ref())?)?,
        "drainErrors" => Value::Array(sm.drain_errors().iter().map(settings_error_to_js).collect()),
        "getTerminalCapabilityOverrides" => capability_overrides_to_js(&sm.get_terminal_capability_overrides()),
        "getSettings" => serde_json::to_value(sm.get_settings_json())?,
        "getGlobalSettings" => serde_json::to_value(sm.get_global_settings_json())?,
        "getProjectSettings" => serde_json::to_value(sm.get_project_settings_json())?,
        "isProjectTrusted" => serde_json::to_value(sm.is_project_trusted())?,
        "setProjectTrusted" => { sm.set_project_trusted(b(0)); Value::Null },
        "applyOverrides" => { sm.apply_overrides_json(&de::<JsonMap>(payload, 0)?); Value::Null },
        "getLastChangelogVersion" => serde_json::to_value(sm.get_last_changelog_version())?,
        "setLastChangelogVersion" => { sm.set_last_changelog_version(&s(0)); Value::Null },
        "getSessionDir" => serde_json::to_value(sm.get_session_dir())?,
        "getDefaultProvider" => serde_json::to_value(sm.get_default_provider())?,
        "getDefaultModel" => serde_json::to_value(sm.get_default_model())?,
        "setDefaultProvider" => { sm.set_default_provider(&s(0)); Value::Null },
        "setDefaultModel" => { sm.set_default_model(&s(0)); Value::Null },
        "setDefaultModelAndProvider" => { sm.set_default_model_and_provider(&s(0), &s(1)); Value::Null },
        "getSteeringMode" => serde_json::to_value(sm.get_steering_mode())?,
        "setSteeringMode" => { sm.set_steering_mode(de::<SettingsManagerGetSteeringModeResult>(payload, 0)?); Value::Null },
        "getFollowUpMode" => serde_json::to_value(sm.get_follow_up_mode())?,
        "setFollowUpMode" => { sm.set_follow_up_mode(de::<SettingsManagerGetSteeringModeResult>(payload, 0)?); Value::Null },
        "getThemeSetting" => serde_json::to_value(sm.get_theme_setting())?,
        "getTheme" => serde_json::to_value(sm.get_theme())?,
        "setTheme" => { sm.set_theme(&s(0)); Value::Null },
        "getDefaultThinkingLevel" => serde_json::to_value(sm.get_default_thinking_level())?,
        "setDefaultThinkingLevel" => { sm.set_default_thinking_level(de::<ThinkingLevel>(payload, 0)?); Value::Null },
        "getAllModelThinkingLevels" => serde_json::to_value(sm.get_all_model_thinking_levels())?,
        "setModelThinkingLevel" => { sm.set_model_thinking_level(&s(0), &s(1), de::<ThinkingLevel>(payload, 2)?); Value::Null },
        "removeModelThinkingLevel" => { sm.remove_model_thinking_level(&s(0), &s(1)); Value::Null },
        "getTransport" => serde_json::to_value(sm.get_transport())?,
        "setTransport" => { sm.set_transport(&de::<TransportSetting>(payload, 0)?); Value::Null },
        "getSessionShareDestination" => serde_json::to_value(sm.get_session_share_destination())?,
        "setSessionShareDestination" => { sm.set_session_share_destination(&de::<SessionShareDestination>(payload, 0)?); Value::Null },
        "getCompactionEnabled" => serde_json::to_value(sm.get_compaction_enabled())?,
        "setCompactionEnabled" => { sm.set_compaction_enabled(b(0)); Value::Null },
        "getBranchSummarySettings" => serde_json::to_value(sm.get_branch_summary_settings())?,
        "getBranchSummarySkipPrompt" => serde_json::to_value(sm.get_branch_summary_skip_prompt())?,
        "getRetryEnabled" => serde_json::to_value(sm.get_retry_enabled())?,
        "setRetryEnabled" => { sm.set_retry_enabled(b(0)); Value::Null },
        "getRetrySettings" => serde_json::to_value(sm.get_retry_settings())?,
        "getHttpIdleTimeoutMs" => serde_json::to_value(sm.get_http_idle_timeout_ms()?)?,
        "setHttpIdleTimeoutMs" => { sm.set_http_idle_timeout_ms(n(0))?; Value::Null },
        "getCacheWarmingMode" => serde_json::to_value(sm.get_cache_warming_mode())?,
        "setCacheWarmingMode" => { sm.set_cache_warming_mode(&de::<CacheWarmingMode>(payload, 0)?); Value::Null },
        "getProviderRetrySettings" => serde_json::to_value(sm.get_provider_retry_settings())?,
        "getWebSocketConnectTimeoutMs" => serde_json::to_value(sm.get_web_socket_connect_timeout_ms()?)?,
        "getHideThinkingBlock" => serde_json::to_value(sm.get_hide_thinking_block())?,
        "getShowCacheMissNotices" => serde_json::to_value(sm.get_show_cache_miss_notices())?,
        "getExternalEditorCommand" => serde_json::to_value(sm.get_external_editor_command())?,
        "setHideThinkingBlock" => { sm.set_hide_thinking_block(b(0)); Value::Null },
        "setShowCacheMissNotices" => { sm.set_show_cache_miss_notices(b(0)); Value::Null },
        "getShellPath" => serde_json::to_value(sm.get_shell_path())?,
        "setShellPath" => { sm.set_shell_path(os(0).as_deref()); Value::Null },
        "getQuietStartup" => serde_json::to_value(sm.get_quiet_startup())?,
        "setQuietStartup" => { sm.set_quiet_startup(&de::<QuietStartup>(payload, 0)?); Value::Null },
        "getDefaultProjectTrust" => serde_json::to_value(sm.get_default_project_trust())?,
        "setDefaultProjectTrust" => { sm.set_default_project_trust(&de::<DefaultProjectTrust>(payload, 0)?); Value::Null },
        "getShellCommandPrefix" => serde_json::to_value(sm.get_shell_command_prefix())?,
        "setShellCommandPrefix" => { sm.set_shell_command_prefix(os(0).as_deref()); Value::Null },
        "getNpmCommand" => serde_json::to_value(sm.get_npm_command())?,
        "setNpmCommand" => { sm.set_npm_command(de::<Option<Vec<String>>>(payload, 0)?.as_deref()); Value::Null },
        "getCollapseChangelog" => serde_json::to_value(sm.get_collapse_changelog())?,
        "setCollapseChangelog" => { sm.set_collapse_changelog(b(0)); Value::Null },
        "getEnableInstallTelemetry" => serde_json::to_value(sm.get_enable_install_telemetry())?,
        "setEnableInstallTelemetry" => { sm.set_enable_install_telemetry(b(0)); Value::Null },
        "getEnableAnalytics" => serde_json::to_value(sm.get_enable_analytics())?,
        "getTrackingId" => serde_json::to_value(sm.get_tracking_id())?,
        "setEnableAnalytics" => { sm.set_enable_analytics(b(0)); Value::Null },
        "getOrCreateDeviceId" => serde_json::to_value(sm.get_or_create_device_id())?,
        "getPackages" => serde_json::to_value(sm.get_packages())?,
        "setPackages" => { sm.set_packages(&de::<Vec<PackageSource>>(payload, 0)?); Value::Null },
        "setProjectPackages" => { sm.set_project_packages(&de::<Vec<PackageSource>>(payload, 0)?)?; Value::Null },
        "getExtensionPaths" => serde_json::to_value(sm.get_extension_paths())?,
        "setExtensionPaths" => { sm.set_extension_paths(&de::<Vec<String>>(payload, 0)?); Value::Null },
        "setProjectExtensionPaths" => { sm.set_project_extension_paths(&de::<Vec<String>>(payload, 0)?)?; Value::Null },
        "getSkillPaths" => serde_json::to_value(sm.get_skill_paths())?,
        "setSkillPaths" => { sm.set_skill_paths(&de::<Vec<String>>(payload, 0)?); Value::Null },
        "setProjectSkillPaths" => { sm.set_project_skill_paths(&de::<Vec<String>>(payload, 0)?)?; Value::Null },
        "getPromptTemplatePaths" => serde_json::to_value(sm.get_prompt_template_paths())?,
        "setPromptTemplatePaths" => { sm.set_prompt_template_paths(&de::<Vec<String>>(payload, 0)?); Value::Null },
        "setProjectPromptTemplatePaths" => { sm.set_project_prompt_template_paths(&de::<Vec<String>>(payload, 0)?)?; Value::Null },
        "getThemePaths" => serde_json::to_value(sm.get_theme_paths())?,
        "setThemePaths" => { sm.set_theme_paths(&de::<Vec<String>>(payload, 0)?); Value::Null },
        "setProjectThemePaths" => { sm.set_project_theme_paths(&de::<Vec<String>>(payload, 0)?)?; Value::Null },
        "getEnableSkillCommands" => serde_json::to_value(sm.get_enable_skill_commands())?,
        "setEnableSkillCommands" => { sm.set_enable_skill_commands(b(0)); Value::Null },
        "getThinkingBudgets" => serde_json::to_value(sm.get_thinking_budgets())?,
        "getShowImages" => serde_json::to_value(sm.get_show_images())?,
        "setShowImages" => { sm.set_show_images(b(0)); Value::Null },
        "getImageWidthCells" => serde_json::to_value(sm.get_image_width_cells())?,
        "setImageWidthCells" => { sm.set_image_width_cells(n(0)); Value::Null },
        "getClearOnShrink" => serde_json::to_value(sm.get_clear_on_shrink())?,
        "setClearOnShrink" => { sm.set_clear_on_shrink(b(0)); Value::Null },
        "getShowTerminalProgress" => serde_json::to_value(sm.get_show_terminal_progress())?,
        "setShowTerminalProgress" => { sm.set_show_terminal_progress(b(0)); Value::Null },
        "getTuiMode" => serde_json::to_value(sm.get_tui_mode())?,
        "setTuiMode" => { sm.set_tui_mode(&de::<TuiMode>(payload, 0)?); Value::Null },
        "getFullscreenExitOutput" => serde_json::to_value(sm.get_fullscreen_exit_output())?,
        "setFullscreenExitOutput" => { sm.set_fullscreen_exit_output(&de::<FullscreenExitOutput>(payload, 0)?); Value::Null },
        "getFullscreenScrollbar" => json!(match sm.get_fullscreen_scrollbar() {
            ScrollViewScrollbar::Hidden => "hidden",
            ScrollViewScrollbar::Auto => "auto",
            ScrollViewScrollbar::Always => "always",
        }),
        "setFullscreenScrollbar" => {
            sm.set_fullscreen_scrollbar(match arg(payload, 0).as_str() {
                Some("always") => ScrollViewScrollbar::Always,
                Some("auto") => ScrollViewScrollbar::Auto,
                _ => ScrollViewScrollbar::Hidden,
            });
            Value::Null
        }
        "getFullscreenCopyOnSelect" => serde_json::to_value(sm.get_fullscreen_copy_on_select())?,
        "setFullscreenCopyOnSelect" => { sm.set_fullscreen_copy_on_select(b(0)); Value::Null },
        "getFullscreenWheelScrollLines" => serde_json::to_value(sm.get_fullscreen_wheel_scroll_lines())?,
        "setFullscreenWheelScrollLines" => { sm.set_fullscreen_wheel_scroll_lines(&de::<WheelScrollLines>(payload, 0)?); Value::Null },
        "getImageAutoResize" => serde_json::to_value(sm.get_image_auto_resize())?,
        "setImageAutoResize" => { sm.set_image_auto_resize(b(0)); Value::Null },
        "getBlockImages" => serde_json::to_value(sm.get_block_images())?,
        "setBlockImages" => { sm.set_block_images(b(0)); Value::Null },
        "getEnabledModels" => serde_json::to_value(sm.get_enabled_models())?,
        "getDefaultTools" => serde_json::to_value(sm.get_default_tools())?,
        "setEnabledModels" => { sm.set_enabled_models(de::<Option<Vec<String>>>(payload, 0)?.as_deref()); Value::Null },
        "getDoubleEscapeAction" => serde_json::to_value(sm.get_double_escape_action())?,
        "setDoubleEscapeAction" => { sm.set_double_escape_action(de::<SettingsManagerGetDoubleEscapeActionResult>(payload, 0)?); Value::Null },
        "getTreeFilterMode" => serde_json::to_value(sm.get_tree_filter_mode())?,
        "setTreeFilterMode" => { sm.set_tree_filter_mode(de::<FilterMode>(payload, 0)?); Value::Null },
        "getShowHardwareCursor" => serde_json::to_value(sm.get_show_hardware_cursor())?,
        "setShowHardwareCursor" => { sm.set_show_hardware_cursor(b(0)); Value::Null },
        "getEditorPaddingX" => serde_json::to_value(sm.get_editor_padding_x())?,
        "setEditorPaddingX" => { sm.set_editor_padding_x(n(0)); Value::Null },
        "getOutputPad" => serde_json::to_value(sm.get_output_pad())?,
        "setOutputPad" => { sm.set_output_pad(n(0)); Value::Null },
        "getAutocompleteMaxVisible" => serde_json::to_value(sm.get_autocomplete_max_visible())?,
        "setAutocompleteMaxVisible" => { sm.set_autocomplete_max_visible(n(0)); Value::Null },
        "getCodeBlockIndent" => serde_json::to_value(sm.get_code_block_indent())?,
        "getMermaidRenderingMode" => serde_json::to_value(sm.get_mermaid_rendering_mode())?,
        "setMermaidRenderingMode" => { sm.set_mermaid_rendering_mode(&de::<MermaidRenderingMode>(payload, 0)?); Value::Null },
        "getWarnings" => serde_json::to_value(sm.get_warnings())?,
        "setWarnings" => { sm.set_warnings(&de::<WarningSettings>(payload, 0)?); Value::Null },
        _ => return Err(anyhow::anyhow!("settingsManager.{method} is not a function")),
    })
}

async fn settings_async(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let sm = settings_of(bridge, payload)?;
    let method = str_arg(payload, "m");
    Ok(match method.as_str() {
        "reload" => { sm.reload().await?; Value::Null },
        "flush" => { sm.flush().await?; Value::Null },

        _ => return Err(anyhow::anyhow!("settingsManager.{method} is not a function")),
    })
}

// ---------------------------------------------------------------------------
// ModelRuntime
// ---------------------------------------------------------------------------

use crate::core::model_runtime::{
    CreateModelRuntimeOptions, ModelRuntime, ModelRuntimeAuthOverrides, ModelRuntimeGetAuthProviderOrModel,
    ModelRuntimeResolveModelOptions,
};

/// The JS id of a model runtime; one id (one JS object) per runtime.
pub(crate) fn runtime_id(bridge: &Bridge, runtime: &Arc<ModelRuntime>) -> u64 {
    let key = Arc::as_ptr(runtime) as usize;
    let mut ids = bridge.model_runtime_ids.lock();
    if let Some(id) = ids.get(&key)
        && bridge.model_runtimes.get(*id).is_some()
    {
        return *id;
    }
    let id = bridge.model_runtimes.insert(runtime.clone());
    ids.insert(key, id);
    id
}

pub(crate) fn runtime_of(bridge: &Bridge, payload: &Value) -> anyhow::Result<Arc<ModelRuntime>> {
    bridge
        .model_runtimes
        .get(payload["runtime"].as_u64().unwrap_or_default())
        .ok_or_else(|| anyhow::anyhow!("This model runtime is no longer available"))
}

fn auth_options(bridge: &Bridge, value: &Value) -> crate::compat::AuthOperationOptions {
    crate::compat::AuthOperationOptions { signal: bridge.token(&value["signal"]) }
}

fn opt_str(value: &Value) -> Option<&str> {
    value.as_str()
}

fn request_options(bridge: &Bridge, value: &Value) -> Option<pi_ai::models::ModelsRequestOptions> {
    (!value.is_null()).then(|| super::provider::stream_options_from_json(bridge, value).into())
}

fn runtime_call(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let method = str_arg(payload, "m");
    if method == "release" {
        bridge.model_runtimes.remove(payload["runtime"].as_u64().unwrap_or_default());
        return Ok(Value::Null);
    }
    let runtime = runtime_of(bridge, payload)?;
    let s = |i: usize| arg(payload, i).as_str().unwrap_or_default().to_string();
    let a = |i: usize| arg(payload, i);
    fn de<T: serde::de::DeserializeOwned>(value: Value) -> anyhow::Result<T> {
        serde_json::from_value(value).map_err(|e| anyhow::anyhow!("Invalid argument: {e}"))
    }
    let provider = |p: Option<Arc<dyn pi_ai::models::Provider>>| match p {
        Some(p) => super::native::provider_to_js(bridge, p),
        None => Value::Null,
    };
    Ok(match method.as_str() {
        "getProviders" => Value::Array(
            runtime.get_providers().into_iter().map(|p| super::native::provider_to_js(bridge, p)).collect(),
        ),
        "getProvider" => provider(runtime.get_provider(&s(0))),
        "getModels" => serde_json::to_value(runtime.get_models(opt_str(&a(0))))?,
        "getModel" => serde_json::to_value(runtime.get_model(&s(0), &s(1)))?,
        "getModelsOfType" => serde_json::to_value(runtime.get_models_of_type(de(a(0))?, opt_str(&a(1))))?,
        "getModelOfType" => serde_json::to_value(runtime.get_model_of_type(de(a(0))?, &s(1), &s(2)))?,
        "getAllModels" => serde_json::to_value(runtime.get_all_models(opt_str(&a(0))))?,
        "getAvailableSnapshot" => serde_json::to_value(runtime.get_available_snapshot())?,
        "getError" => json!(runtime.get_error()),
        "getRegisteredProviderConfig" => match runtime.get_registered_provider_config_revision(&s(0)) {
            Some((rev, config)) => json!({ "rev": rev, "config": super::native::provider_config_to_js(bridge, &config) }),
            None => Value::Null,
        },
        "getRegisteredProviderIds" => json!(runtime.get_registered_provider_ids()),
        "getRegisteredNativeProvider" => provider(runtime.get_registered_native_provider(&s(0))),
        "getCompatibilityRequestConfig" => {
            let config = runtime.get_compatibility_request_config(&de(a(0))?)?;
            let mut out = json!({ "authHeader": config.auth_header });
            if let Some(h) = &config.headers {
                out["headers"] = serde_json::to_value(h)?;
            }
            out
        }
        "isUsingOAuth" => json!(runtime.is_using_o_auth(&s(0))),
        "isUsingSubscription" => json!(runtime.is_using_subscription(&s(0))),
        "hasConfiguredAuth" => json!(runtime.has_configured_auth(&s(0))),
        "getProviderAuthStatus" => serde_json::to_value(runtime.get_provider_auth_status(&s(0)))?,
        "registerNativeProvider" => {
            runtime.register_native_provider(super::native::native_provider(bridge, &a(0))?)?;
            Value::Null
        }
        "registerProvider" => {
            let config = super::provider::js_provider_config(bridge, &a(1))?;
            runtime.register_provider(&s(0), &config)?;
            Value::Null
        }
        "unregisterProvider" => {
            runtime.unregister_provider(&s(0));
            Value::Null
        }
        "registerVirtualModel" => {
            runtime.register_virtual_model(&super::provider::js_virtual_model_definition(bridge, &a(0))?)?;
            Value::Null
        }
        "unregisterVirtualModel" => {
            runtime.unregister_virtual_model(&s(0), &s(1));
            Value::Null
        }
        "getPhysicalModel" => serde_json::to_value(runtime.get_physical_model(&s(0), &s(1)))?,
        _ => return Err(anyhow::anyhow!("modelRuntime.{method} is not a function")),
    })
}

fn runtime_stream(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let runtime = runtime_of(bridge, payload)?;
    let method = str_arg(payload, "m");
    let model = serde_json::from_value(arg(payload, 0))?;
    let options = request_options(bridge, &arg(payload, 2));
    let stream = match method.as_str() {
        "streamDeferred" => runtime.stream_deferred(model, serde_json::from_value(arg(payload, 1))?, options),
        "streamSimple" => runtime.stream_simple(model, serde_json::from_value(arg(payload, 1))?, options),
        _ => runtime.stream(model, serde_json::from_value(arg(payload, 1))?, options),
    };
    Ok(json!(super::provider::register_stream(bridge, stream)))
}

async fn runtime_async(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let method = str_arg(payload, "m");
    if method == "create" {
        let o = arg(payload, 0);
        let options = CreateModelRuntimeOptions {
            auth_path: o["authPath"].as_str().map(str::to_string),
            // TS `modelsPath: null` disables models.json (the Rust runtime takes an empty path).
            models_path: if o.get("modelsPath").is_some_and(Value::is_null) {
                Some(String::new())
            } else {
                o["modelsPath"].as_str().map(str::to_string)
            },
            models_store_path: o["modelsStorePath"].as_str().map(str::to_string),
            allow_model_network: o["allowModelNetwork"].as_bool(),
            model_refresh_timeout_ms: o["modelRefreshTimeoutMs"].as_f64().map(|v| v as i64),
            catalog_base_url: o["catalogBaseUrl"].as_str().map(str::to_string),
            signal: bridge.token(&o["signal"]),
            refresh_on_create: o["refreshOnCreate"].as_bool(),
            ..Default::default()
        };
        let runtime = ModelRuntime::create(Some(&options)).await?;
        return Ok(json!(runtime_id(bridge, &runtime)));
    }
    let runtime = runtime_of(bridge, payload)?;
    let s = |i: usize| arg(payload, i).as_str().unwrap_or_default().to_string();
    let a = |i: usize| arg(payload, i);
    Ok(match method.as_str() {
        "getAvailableOfType" => serde_json::to_value(
            runtime
                .get_available_of_type(serde_json::from_value(a(0))?, opt_str(&a(1)), Some(&auth_options(bridge, &a(2))))
                .await?,
        )?,
        "getAllAvailable" => {
            serde_json::to_value(runtime.get_all_available(opt_str(&a(0)), Some(&auth_options(bridge, &a(1)))).await?)?
        }
        "checkAuth" => match runtime.check_auth(&s(0), Some(&auth_options(bridge, &a(1)))).await? {
            Some(check) => super::native::auth_check_to_js(&check),
            None => Value::Null,
        },
        "getAvailable" => {
            serde_json::to_value(runtime.get_available(opt_str(&a(0)), Some(&auth_options(bridge, &a(1)))).await?)?
        }
        "getAuth" => {
            let target = match a(0) {
                Value::String(id) => ModelRuntimeGetAuthProviderOrModel::String(id),
                model => ModelRuntimeGetAuthProviderOrModel::AnyModel(serde_json::from_value(model)?),
            };
            let o = a(1);
            let overrides = ModelRuntimeAuthOverrides {
                signal: bridge.token(&o["signal"]),
                api_key: o["apiKey"].as_str().map(str::to_string),
                env: super::api::opt(&o["env"])?,
                min_o_auth_validity_ms: o["minOAuthValidityMs"].as_f64().map(|v| v as i64),
            };
            match runtime.get_auth(&target, Some(&overrides)).await? {
                Some(auth) => super::native::auth_result_to_js(&auth),
                None => Value::Null,
            }
        }
        "setRuntimeApiKey" => {
            runtime.set_runtime_api_key(&s(0), &s(1), Some(&auth_options(bridge, &a(2)))).await?;
            Value::Null
        }
        "removeRuntimeApiKey" => {
            runtime.remove_runtime_api_key(&s(0), Some(&auth_options(bridge, &a(1)))).await?;
            Value::Null
        }
        "listCredentials" => serde_json::to_value(runtime.list_credentials(Some(&auth_options(bridge, &a(0)))).await?)?,
        "complete" | "completeSimple" => {
            let model = serde_json::from_value(a(0))?;
            let context = serde_json::from_value(a(1))?;
            let options = request_options(bridge, &a(2));
            let message = if method == "complete" {
                runtime.complete(model, context, options).await
            } else {
                runtime.complete_simple(model, context, options).await
            };
            serde_json::to_value(message)?
        }
        "fetchDeferred" => serde_json::to_value(
            runtime.fetch_deferred(serde_json::from_value(a(0))?, serde_json::from_value(a(1))?, request_options(bridge, &a(2))).await,
        )?,
        "cancelDeferred" => {
            runtime
                .cancel_deferred(serde_json::from_value(a(0))?, serde_json::from_value(a(1))?, request_options(bridge, &a(2)))
                .await?;
            Value::Null
        }
        "generateImages" => serde_json::to_value(
            runtime.generate_images(serde_json::from_value(a(0))?, serde_json::from_value(a(1))?, request_options(bridge, &a(2))).await,
        )?,
        "classify" => serde_json::to_value(
            runtime.classify(serde_json::from_value(a(0))?, serde_json::from_value(a(1))?, request_options(bridge, &a(2))).await,
        )?,
        "login" => {
            let interaction = super::native::js_interaction(bridge, &json!({ "interaction": a(2), "signal": payload["signal"] }))?;
            let o = a(3);
            let options = o.is_object().then(|| pi_ai::auth::types::LoginOptions {
                get_device_id: bridge.handle(&o["getDeviceId"]).map(|h| {
                    Arc::new(move || h.call_blocking(vec![]).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default())
                        as Arc<dyn Fn() -> String + Send + Sync>
                }),
                agent_name: o["agentName"].as_str().map(str::to_string),
            });
            let credential = runtime
                .login(&s(0), serde_json::from_value(a(1))?, interaction.interaction, Some(interaction.signal), options)
                .await?;
            serde_json::to_value(credential)?
        }
        "logout" => {
            runtime.logout(&s(0), Some(&auth_options(bridge, &a(1)))).await?;
            Value::Null
        }
        "refresh" => {
            let o = a(0);
            let result = runtime
                .refresh(Some(pi_ai::models::ModelsRefreshOptions {
                    allow_network: o["allowNetwork"].as_bool(),
                    providers: super::api::opt(&o["providers"])?,
                    force: o["force"].as_bool(),
                    signal: bridge.token(&o["signal"]),
                }))
                .await?;
            json!({ "aborted": result.aborted, "errors": result.errors })
        }
        "resolveModel" => {
            let o = a(2);
            let options = ModelRuntimeResolveModelOptions {
                reason: serde_json::from_value(o["reason"].clone())?,
                thinking_level: serde_json::from_value(o["thinkingLevel"].clone())?,
                signal: bridge.token(&o["signal"]),
                failed: super::api::opt(&o["failed"])?,
                state: o.get("state").filter(|v| !v.is_null()).cloned(),
            };
            let messages: Vec<pi_ai::types::Message> = serde_json::from_value(a(1))?;
            serde_json::to_value(runtime.resolve_model(serde_json::from_value(a(0))?, &messages, &options).await?)?
        }
        _ => return Err(anyhow::anyhow!("modelRuntime.{method} is not a function")),
    })
}

// ---------------------------------------------------------------------------
// Extension factories (DefaultResourceLoader `extensionFactories`, built-in factories)
// ---------------------------------------------------------------------------

use crate::core::extensions::types::{
    ExtensionAPIRef, ExtensionFactory, InlineExtension, LoadExtensionsResult, NamedInlineExtension,
};

/// A JavaScript extension factory `(pi) => void | Promise<void>` as a Rust factory.
pub(crate) fn js_extension_factory(bridge: &Arc<Bridge>, handle: pi_jsrt::JsHandle) -> ExtensionFactory {
    let bridge = bridge.clone();
    Arc::new(move |api: ExtensionAPIRef| {
        let bridge = bridge.clone();
        let handle = handle.clone();
        Box::pin(async move {
            let api_id = bridge.apis.insert(api);
            bridge.glue_call("runFactory", vec![handle.to_value(), json!(api_id)]).await?;
            Ok(())
        })
    })
}

/// A Rust built-in extension factory (`createCodemodeExtension()` and friends).
fn builtin_extension_factory(name: &str, options: &Value) -> anyhow::Result<ExtensionFactory> {
    Ok(match name {
        "codemode" => {
            let options: Option<crate::extensions::codemode::CodemodeExtensionOptions> = super::api::opt(options)?;
            crate::extensions::codemode::create_codemode_extension(options.as_ref())
        }
        "tool-search" => crate::extensions::tool_search::create_tool_search_extension(),
        "mcp" => {
            let mut mcp = crate::extensions::mcp::McpExtensionOptions::default();
            if let Some(path) = options["logPath"].as_str() {
                mcp.log_path = Some(path.to_string());
            }
            crate::extensions::mcp::create_mcp_extension(Some(&mcp))?
        }
        other => anyhow::bail!("unknown built-in extension {other}"),
    })
}

/// An extension factory value from JavaScript: a function, or a built-in factory marker.
fn factory_from_js(bridge: &Arc<Bridge>, value: &Value) -> anyhow::Result<ExtensionFactory> {
    if let Some(name) = value["$piBuiltinExtension"].as_str() {
        return builtin_extension_factory(name, &value["options"]);
    }
    let handle = bridge.handle(value).ok_or_else(|| anyhow::anyhow!("extension factory must be a function"))?;
    Ok(js_extension_factory(bridge, handle))
}

/// `createCodemodeExtension()(pi)` called directly by an extension: run the Rust built-in factory
/// against that extension's API.
async fn run_builtin_extension(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let api = bridge
        .apis
        .get(payload["api"].as_u64().unwrap_or_default())
        .ok_or_else(|| anyhow::anyhow!("built-in extension factories expect the pi object of an extension"))?;
    let factory = builtin_extension_factory(&str_arg(payload, "name"), &payload["options"])?;
    factory(api).await?;
    Ok(Value::Null)
}

// ---------------------------------------------------------------------------
// DefaultResourceLoader
// ---------------------------------------------------------------------------

use crate::core::resource_loader::{DefaultResourceLoader, DefaultResourceLoaderOptions, ResourceLoader};

fn call_js_blocking<T: serde::de::DeserializeOwned>(handle: &pi_jsrt::JsHandle, arg: Value) -> Option<T> {
    let value = handle.call_blocking(vec![arg]).ok()?;
    serde_json::from_value(value).ok()
}

/// A loaded extension as the TS `Extension` object JavaScript sees (`index` maps it back).
fn extension_to_js(bridge: &Bridge, index: usize, extension: &crate::core::extensions::types::ExtensionRef) -> Value {
    let extension_id = extension_id(bridge, extension);
    let e = extension.read();
    let source = |s: &crate::core::source_info::SourceInfo| serde_json::to_value(s).unwrap_or(Value::Null);
    let tools: serde_json::Map<String, Value> = e
        .tools
        .iter()
        .map(|(name, tool)| {
            let d = &tool.definition;
            let mut definition = json!({
                "name": d.name,
                "label": d.label,
                "description": d.description,
                "parameters": d.parameters,
            });
            if let Some(p) = &d.prompt_snippet {
                definition["promptSnippet"] = json!(p);
            }
            if let Some(g) = &d.prompt_guidelines {
                definition["promptGuidelines"] = json!(g);
            }
            if let Some(a) = d.default_active {
                definition["defaultActive"] = json!(a);
            }
            if let Some(x) = &d.exposure {
                definition["exposure"] = serde_json::to_value(x).unwrap_or(Value::Null);
            }
            (name.clone(), json!({ "definition": definition, "sourceInfo": source(&tool.source_info) }))
        })
        .collect();
    let commands: serde_json::Map<String, Value> = e
        .commands
        .iter()
        .map(|(name, c)| {
            let mut v = json!({ "name": c.name, "sourceInfo": source(&c.source_info) });
            if let Some(d) = &c.description {
                v["description"] = json!(d);
            }
            (name.clone(), v)
        })
        .collect();
    let flags: serde_json::Map<String, Value> =
        e.flags.iter().map(|(name, f)| (name.clone(), serde_json::to_value(f).unwrap_or(Value::Null))).collect();
    let shortcuts: serde_json::Map<String, Value> = e
        .shortcuts
        .iter()
        .map(|(key, s)| {
            let mut v = json!({ "shortcut": s.shortcut, "extensionPath": s.extension_path });
            if let Some(d) = &s.description {
                v["description"] = json!(d);
            }
            (key.to_string(), v)
        })
        .collect();
    let handlers: serde_json::Map<String, Value> =
        e.handlers.iter().map(|(event, list)| (event.clone(), json!(list.len()))).collect();
    let mut out = json!({
        "$index": index,
        "$extension": extension_id,
        "path": e.path,
        "resolvedPath": e.resolved_path,
        "sourceInfo": source(&e.source_info),
        "tools": tools,
        "commands": commands,
        "flags": flags,
        "shortcuts": shortcuts,
        "handlers": handlers,
        "messageRenderers": e.message_renderers.keys().collect::<Vec<_>>(),
    });
    if let Some(h) = e.hidden {
        out["hidden"] = json!(h);
    }
    if let Some(r) = e.replaceable {
        out["replaceable"] = json!(r);
    }
    out
}

/// `LoadExtensionsResult` as JavaScript sees it (`extensionsResultFromHost` in pi-sdk.js).
pub(crate) fn extensions_result_to_js(bridge: &Bridge, result: &LoadExtensionsResult) -> Value {
    let mut out = json!({
        "extensions": result.extensions.iter().enumerate().map(|(i, e)| extension_to_js(bridge, i, e)).collect::<Vec<_>>(),
        "errors": serde_json::to_value(&result.errors).unwrap_or(Value::Null),
        "$runtime": ext_runtime_id(bridge, &result.runtime),
    });
    if let Some(w) = &result.warnings {
        out["warnings"] = serde_json::to_value(w).unwrap_or(Value::Null);
    }
    out
}

fn loader_options(bridge: &Arc<Bridge>, o: &Value) -> anyhow::Result<DefaultResourceLoaderOptions> {
    let opt_paths = |key: &str| -> anyhow::Result<Option<Vec<String>>> { super::api::opt(&o[key]) };
    let mut options = DefaultResourceLoaderOptions {
        cwd: o["cwd"].as_str().map(str::to_string).unwrap_or_else(|| bridge.cwd.clone()),
        agent_dir: o["agentDir"].as_str().map(str::to_string).unwrap_or_else(crate::config::get_agent_dir),
        settings_manager: match o["settingsManager"].as_u64() {
            Some(id) => Some(bridge.settings_managers.get(id).ok_or_else(|| anyhow::anyhow!("unknown settings manager"))?),
            None => None,
        },
        additional_extension_paths: opt_paths("additionalExtensionPaths")?,
        additional_skill_paths: opt_paths("additionalSkillPaths")?,
        additional_prompt_template_paths: opt_paths("additionalPromptTemplatePaths")?,
        additional_theme_paths: opt_paths("additionalThemePaths")?,
        no_extensions: o["noExtensions"].as_bool(),
        disabled_builtin_extensions: opt_paths("disabledBuiltinExtensions")?,
        no_skills: o["noSkills"].as_bool(),
        no_prompt_templates: o["noPromptTemplates"].as_bool(),
        no_themes: o["noThemes"].as_bool(),
        no_context_files: o["noContextFiles"].as_bool(),
        system_prompt: o["systemPrompt"].as_str().map(str::to_string),
        append_system_prompt: match &o["appendSystemPrompt"] {
            Value::String(s) => Some(vec![s.clone()]),
            other => super::api::opt(other)?,
        },
        ..Default::default()
    };
    if let Some(entries) = o["extensionFactories"].as_array() {
        let mut factories = Vec::new();
        for entry in entries {
            if entry.get("factory").is_some() {
                factories.push(InlineExtension::Named(NamedInlineExtension {
                    name: str_arg(entry, "name"),
                    factory: factory_from_js(bridge, &entry["factory"])?,
                    hidden: entry["hidden"].as_bool(),
                    replaceable: entry["replaceable"].as_bool(),
                    builtin: entry["builtin"].as_bool(),
                }));
            } else {
                factories.push(InlineExtension::Factory(factory_from_js(bridge, entry)?));
            }
        }
        options.extension_factories = Some(factories);
    }
    if let Some(h) = bridge.handle(&o["systemPromptOverride"]) {
        options.system_prompt_override = Some(Arc::new(move |base: Option<String>| {
            h.call_blocking(vec![json!(base)]).ok().and_then(|v| v.as_str().map(str::to_string))
        }));
    }
    if let Some(h) = bridge.handle(&o["appendSystemPromptOverride"]) {
        options.append_system_prompt_override = Some(Arc::new(move |base: Vec<String>| {
            call_js_blocking(&h, json!(base.clone())).unwrap_or(base)
        }));
    }
    if let Some(h) = bridge.handle(&o["skillsOverride"]) {
        options.skills_override = Some(Arc::new(move |base| {
            let value = serde_json::to_value(&base).unwrap_or(Value::Null);
            call_js_blocking(&h, value).unwrap_or(base)
        }));
    }
    if let Some(h) = bridge.handle(&o["promptsOverride"]) {
        options.prompts_override = Some(Arc::new(move |base| {
            let value = serde_json::to_value(&base).unwrap_or(Value::Null);
            call_js_blocking(&h, value).unwrap_or(base)
        }));
    }
    if let Some(h) = bridge.handle(&o["agentsFilesOverride"]) {
        options.agents_files_override = Some(Arc::new(move |base| {
            let value = serde_json::to_value(&base).unwrap_or(Value::Null);
            call_js_blocking(&h, value).unwrap_or(base)
        }));
    }
    if let Some(h) = bridge.handle(&o["extensionsOverride"]) {
        let b = Arc::downgrade(bridge);
        options.extensions_override = Some(Arc::new(move |base: LoadExtensionsResult| {
            let Some(bridge) = b.upgrade() else { return base };
            let Ok(value) = bridge.glue_call_blocking("invokeExtensionsOverride", vec![h.to_value(), extensions_result_to_js(&bridge, &base)])
            else {
                return base;
            };
            let extensions = value["extensions"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|e| e["$index"].as_u64().and_then(|i| base.extensions.get(i as usize).cloned()))
                        .collect()
                })
                .unwrap_or_else(|| base.extensions.clone());
            let errors = super::api::opt(&value["errors"]).ok().flatten().unwrap_or_else(|| base.errors.clone());
            LoadExtensionsResult { extensions, errors, warnings: base.warnings.clone(), runtime: base.runtime.clone() }
        }));
    }
    Ok(options)
}

fn loader_of(bridge: &Bridge, payload: &Value) -> anyhow::Result<Arc<DefaultResourceLoader>> {
    bridge
        .resource_loaders
        .get(payload["id"].as_u64().unwrap_or_default())
        .ok_or_else(|| anyhow::anyhow!("This resource loader is no longer available"))
}

/// `reload({ resolveProjectTrust })` of a JS-created DefaultResourceLoader.
fn reload_options(bridge: &Arc<Bridge>, value: &Value) -> Option<crate::core::resource_loader::ResourceLoaderReloadOptions> {
    let handle = bridge.handle(value)?;
    let bridge = Arc::downgrade(bridge);
    let resolve: crate::core::resource_loader::ResolveProjectTrustFn = Arc::new(move |input| {
        let (bridge, handle) = (bridge.clone(), handle.clone());
        Box::pin(async move {
            let bridge = bridge.upgrade().ok_or_else(|| anyhow::anyhow!("extension runtime stopped"))?;
            let result = extensions_result_to_js(&bridge, &input.extensions_result);
            let trusted = bridge.glue_call("invokeResolveProjectTrust", vec![handle.to_value(), result]).await?;
            Ok(trusted.as_bool().unwrap_or(false))
        })
    });
    Some(crate::core::resource_loader::ResourceLoaderReloadOptions { resolve_project_trust: Some(resolve) })
}

fn loader_call(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let method = str_arg(payload, "m");
    match method.as_str() {
        "new" => {
            let options = loader_options(bridge, &arg(payload, 0))?;
            return Ok(json!(bridge.resource_loaders.insert(Arc::new(DefaultResourceLoader::new(&options)))));
        }
        "release" => {
            bridge.resource_loaders.remove(payload["id"].as_u64().unwrap_or_default());
            return Ok(Value::Null);
        }
        _ => {}
    }
    let loader = loader_of(bridge, payload)?;
    Ok(match method.as_str() {
        "getExtensions" => extensions_result_to_js(bridge, &loader.get_extensions()),
        "getSkills" => serde_json::to_value(loader.get_skills())?,
        "getPrompts" => serde_json::to_value(loader.get_prompts())?,
        "getThemes" => {
            let themes = loader.get_themes();
            json!({
                "themes": themes.themes.iter().map(|t| json!({ "name": t.name(), "sourcePath": t.source_path() })).collect::<Vec<_>>(),
                "diagnostics": serde_json::to_value(&themes.diagnostics)?,
            })
        }
        "getAgentsFiles" => serde_json::to_value(loader.get_agents_files())?,
        "getSystemPrompt" => json!(loader.get_system_prompt()),
        "getAppendSystemPrompt" => json!(loader.get_append_system_prompt()),
        "extendResources" => {
            loader.extend_resources(&serde_json::from_value(arg(payload, 0))?);
            Value::Null
        }
        _ => return Err(anyhow::anyhow!("resourceLoader.{method} is not a function")),
    })
}

// ---------------------------------------------------------------------------
// createAgentSession and AgentSession
// ---------------------------------------------------------------------------

use crate::core::agent_session::{AgentSession, ExtensionBindings, ModelMutationOptions, PromptOptions};
use crate::core::sdk::{CreateAgentSessionOptions, create_agent_session};

fn session_of(bridge: &Bridge, payload: &Value) -> anyhow::Result<Arc<AgentSession>> {
    bridge
        .agent_sessions
        .get(payload["id"].as_u64().unwrap_or_default())
        .ok_or_else(|| anyhow::anyhow!("This agent session is no longer available"))
}

/// The JS id of a settings manager the host owns (one id per manager).
fn settings_id(bridge: &Bridge, settings: &Arc<SettingsManager>) -> u64 {
    let key = Arc::as_ptr(settings) as usize;
    let mut ids = bridge.settings_manager_ids.lock();
    if let Some(id) = ids.get(&key)
        && bridge.settings_managers.get(*id).is_some_and(|s| Arc::ptr_eq(&s, settings))
    {
        return *id;
    }
    let id = bridge.settings_managers.insert(settings.clone());
    ids.insert(key, id);
    id
}

fn agent_state_to_js(state: &pi_agent::types::AgentState) -> Value {
    let tools: Vec<Value> = state
        .tools
        .iter()
        .map(|t| json!({ "name": t.name(), "label": t.label(), "description": t.description(), "parameters": t.parameters() }))
        .collect();
    let mut out = json!({
        "model": serde_json::to_value(&state.model).unwrap_or(Value::Null),
        "thinkingLevel": serde_json::to_value(state.thinking_level).unwrap_or(Value::Null),
        "tools": tools,
        "messages": serde_json::to_value(&state.messages).unwrap_or(Value::Null),
        "isStreaming": state.is_streaming,
        "pendingToolCalls": state.pending_tool_calls.iter().collect::<Vec<_>>(),
    });
    if let Some(m) = &state.streaming_message {
        out["streamingMessage"] = serde_json::to_value(m).unwrap_or(Value::Null);
    }
    if let Some(e) = &state.error_message {
        out["errorMessage"] = json!(e);
    }
    out
}

fn prompt_options(value: &Value) -> anyhow::Result<PromptOptions> {
    Ok(PromptOptions {
        expand_prompt_templates: value["expandPromptTemplates"].as_bool(),
        images: super::api::opt(&value["images"])?,
        streaming_behavior: super::api::opt(&value["streamingBehavior"])?,
        source: super::api::opt(&value["source"])?,
        preflight_result: None,
    })
}

fn create_options(bridge: &Arc<Bridge>, o: &Value) -> anyhow::Result<CreateAgentSessionOptions> {
    let model_runtime = match o["modelRuntime"].as_u64() {
        Some(id) => Some(bridge.model_runtimes.get(id).ok_or_else(|| anyhow::anyhow!("unknown model runtime"))?),
        // The pre-0.80.8 `modelRegistry` option: its runtime.
        None => match &o["modelRegistry"] {
            Value::Null => None,
            target => Some(super::context::registry_of(bridge, target)?.runtime().clone()),
        },
    };
    let custom_tools = match o["customTools"].as_array() {
        Some(tools) => Some(tools.iter().map(|t| super::api::js_tool_definition(bridge, t)).collect::<anyhow::Result<Vec<_>>>()?),
        None => None,
    };
    Ok(CreateAgentSessionOptions {
        cwd: o["cwd"].as_str().map(str::to_string),
        agent_dir: o["agentDir"].as_str().map(str::to_string),
        model_runtime,
        model: super::api::opt(&o["model"])?,
        thinking_level: super::api::opt(&o["thinkingLevel"])?,
        scoped_models: super::api::opt(&o["scopedModels"])?,
        no_tools: super::api::opt(&o["noTools"])?,
        tools: super::api::opt(&o["tools"])?,
        exclude_tools: super::api::opt(&o["excludeTools"])?,
        custom_tools,
        resource_loader: match &o["resourceLoader"] {
            Value::Null => None,
            loader => Some(js_resource_loader(bridge, loader)?),
        },
        session_manager: match o["sessionManager"].as_u64() {
            Some(id) => Some(super::context::session_manager_of(bridge, &json!({ "sm": id }))?),
            None => None,
        },
        settings_manager: match o["settingsManager"].as_u64() {
            Some(id) => Some(bridge.settings_managers.get(id).ok_or_else(|| anyhow::anyhow!("unknown settings manager"))?),
            None => None,
        },
        session_start_event: super::api::opt(&o["sessionStartEvent"])?,
    })
}

fn session_call(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let method = str_arg(payload, "m");
    if method == "release" {
        bridge.agent_sessions.remove(payload["id"].as_u64().unwrap_or_default());
        return Ok(Value::Null);
    }
    let session = session_of(bridge, payload)?;
    if method.starts_with("agent.") {
        return agent_hook_call(bridge, &session, payload["id"].as_u64().unwrap_or_default(), &method, &arg(payload, 0));
    }
    let s = |i: usize| arg(payload, i).as_str().unwrap_or_default().to_string();
    let a = |i: usize| arg(payload, i);
    let mutation = |i: usize| -> anyhow::Result<Option<ModelMutationOptions>> { super::api::opt(&a(i)) };
    Ok(match method.as_str() {
        "subscribe" => {
            let handler = bridge.handle(&a(0)).ok_or_else(|| anyhow::anyhow!("subscribe() expects a listener"))?;
            let listener: crate::core::agent_session::AgentSessionEventListener = Arc::new(move |event| {
                super::api::post_call(&handler, None, vec![serde_json::to_value(&event).unwrap_or(Value::Null)]);
            });
            let unsubscribe = session.subscribe(&listener);
            json!(bridge.unsubscribes.insert(unsubscribe))
        }
        "dispose" => {
            session.dispose();
            Value::Null
        }
        "refreshContext" => {
            session.refresh_context();
            Value::Null
        }
        "createReplacedSessionContext" => {
            json!(bridge.contexts.insert(super::CtxEntry::Replaced(session.create_replaced_session_context())))
        }
        "runner.hasHandlers" => json!(session.has_extension_handlers(&s(0))),
        "runner.createContext" => json!(bridge.contexts.insert(super::CtxEntry::Base(session.extension_runner().create_context()))),
        "runner.createCommandContext" => {
            json!(bridge.contexts.insert(super::CtxEntry::Command(session.extension_runner().create_command_context())))
        }
        "runner.createToolContext" => {
            let context = session.extension_runner().create_tool_context(&s(0), bridge.token(&a(1)));
            json!(bridge.contexts.insert(super::CtxEntry::Tool(context)))
        }
        "state" => agent_state_to_js(&session.state()),
        "cacheWarmingStatus" => serde_json::to_value(session.cache_warming_status())?,
        "setCacheWarmingMode" => {
            session.set_cache_warming_mode(&serde_json::from_value(a(0))?);
            Value::Null
        }
        "model" => serde_json::to_value(session.model())?,
        "thinkingLevel" => serde_json::to_value(session.thinking_level())?,
        "routedModel" => serde_json::to_value(session.routed_model())?,
        "isStreaming" => json!(session.is_streaming()),
        "isIdle" => json!(session.is_idle()),
        "systemPrompt" => json!(session.system_prompt()),
        "retryAttempt" => json!(session.retry_attempt()),
        "getActiveToolNames" => json!(session.get_active_tool_names()),
        "getCallableToolNames" => json!(session.get_callable_tool_names()),
        "getAllTools" => Value::Array(session.get_all_tools().iter().map(super::api::tool_info_json).collect()),
        "getToolDefinition" => match session.get_tool_definition(&s(0)) {
            Some(definition) => super::builtin::tool_definition_to_js(bridge, definition),
            None => Value::Null,
        },
        "setActiveToolsByName" => {
            let names: Vec<String> = serde_json::from_value(a(0))?;
            session.set_active_tools_by_name(&names);
            Value::Null
        }
        "isCompacting" => json!(session.is_compacting()),
        "messages" => serde_json::to_value(session.messages())?,
        "steeringMode" => serde_json::to_value(session.steering_mode())?,
        "followUpMode" => serde_json::to_value(session.follow_up_mode())?,
        "sessionFile" => json!(session.session_file()),
        "sessionId" => json!(session.session_id()),
        "sessionName" => json!(session.session_name()),
        "scopedModels" => serde_json::to_value(session.scoped_models())?,
        "setScopedModels" => {
            let models: Vec<crate::core::agent_session::AgentSessionSetScopedModelsScopedModels> = serde_json::from_value(a(0))?;
            session.set_scoped_models(&models);
            Value::Null
        }
        "promptTemplates" => serde_json::to_value(session.prompt_templates())?,
        "clearQueue" => serde_json::to_value(session.clear_queue())?,
        "pendingMessageCount" => json!(session.pending_message_count()),
        "getSteeringMessages" => json!(session.get_steering_messages()),
        "getFollowUpMessages" => json!(session.get_follow_up_messages()),
        "setThinkingLevel" => {
            session.set_thinking_level(serde_json::from_value(a(0))?, mutation(1)?.as_ref());
            Value::Null
        }
        "cycleThinkingLevel" => serde_json::to_value(session.cycle_thinking_level(mutation(0)?.as_ref()))?,
        "getAvailableThinkingLevels" => serde_json::to_value(session.get_available_thinking_levels())?,
        "supportsThinking" => json!(session.supports_thinking()),
        "setSteeringMode" => {
            session.set_steering_mode(serde_json::from_value(a(0))?);
            Value::Null
        }
        "setFollowUpMode" => {
            session.set_follow_up_mode(serde_json::from_value(a(0))?);
            Value::Null
        }
        "abortCompaction" => {
            session.abort_compaction();
            Value::Null
        }
        "abortBranchSummary" => {
            session.abort_branch_summary();
            Value::Null
        }
        "setAutoCompactionEnabled" => {
            session.set_auto_compaction_enabled(a(0).as_bool().unwrap_or(false));
            Value::Null
        }
        "autoCompactionEnabled" => json!(session.auto_compaction_enabled()),
        "abortRetry" => {
            session.abort_retry();
            Value::Null
        }
        "isRetrying" => json!(session.is_retrying()),
        "autoRetryEnabled" => json!(session.auto_retry_enabled()),
        "setAutoRetryEnabled" => {
            session.set_auto_retry_enabled(a(0).as_bool().unwrap_or(false));
            Value::Null
        }
        "recordBashResult" => {
            session.record_bash_result(&s(0), &serde_json::from_value(a(1))?, super::api::opt(&a(2))?.as_ref());
            Value::Null
        }
        "abortBash" => {
            session.abort_bash();
            Value::Null
        }
        "isBashRunning" => json!(session.is_bash_running()),
        "hasPendingBashMessages" => json!(session.has_pending_bash_messages()),
        "setSessionName" => {
            session.set_session_name(&s(0));
            Value::Null
        }
        "getUserMessagesForForking" => serde_json::to_value(session.get_user_messages_for_forking())?,
        "getSessionStats" => serde_json::to_value(session.get_session_stats())?,
        "getContextUsage" => serde_json::to_value(session.get_context_usage())?,
        "exportToJsonl" => json!(session.export_to_jsonl(a(0).as_str())?),
        "getLastAssistantText" => json!(session.get_last_assistant_text()),
        "hasExtensionHandlers" => json!(session.has_extension_handlers(&s(0))),
        "sessionManager" => json!(super::context::session_manager_id(bridge, &session.session_manager())),
        "settingsManager" => json!(settings_id(bridge, &session.settings_manager())),
        "modelRuntime" => json!(runtime_id(bridge, &session.model_runtime())),
        _ => return Err(anyhow::anyhow!("agentSession.{method} is not a function")),
    })
}

async fn session_async(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let method = str_arg(payload, "m");
    if method == "create" {
        let options = create_options(bridge, &arg(payload, 0))?;
        let result = create_agent_session(Some(&options)).await?;
        let id = bridge.agent_sessions.insert(result.session.clone());
        let mut out = json!({ "session": id, "extensionsResult": extensions_result_to_js(bridge, &result.extensions_result) });
        if let Some(m) = &result.model_fallback_message {
            out["modelFallbackMessage"] = json!(m);
        }
        return Ok(out);
    }
    if method == "agent.callHook" {
        return call_rust_hook(bridge, payload).await;
    }
    let session = session_of(bridge, payload)?;
    let s = |i: usize| arg(payload, i).as_str().unwrap_or_default().to_string();
    let a = |i: usize| arg(payload, i);
    Ok(match method.as_str() {
        "prompt" => {
            session.prompt(&s(0), Some(&prompt_options(&a(1))?)).await?;
            Value::Null
        }
        "steer" | "followUp" => {
            let images: Option<Vec<pi_ai::types::ImageContent>> = super::api::opt(&a(1))?;
            let disposition = if method == "steer" {
                session.steer(&s(0), images.as_deref(), super::api::opt(&a(2))?.as_ref()).await?
            } else {
                session.follow_up(&s(0), images.as_deref(), super::api::opt(&a(2))?.as_ref()).await?
            };
            serde_json::to_value(disposition)?
        }
        "runner.emit" => {
            let event: crate::core::extensions::types::ExtensionEvent = serde_json::from_value(a(0))?;
            serde_json::to_value(session.extension_runner().emit(event).await?)?
        }
        "sendCustomMessage" => {
            session.send_custom_message(&serde_json::from_value(a(0))?, super::api::opt(&a(1))?.as_ref()).await?;
            Value::Null
        }
        "sendUserMessage" => {
            session.send_user_message(serde_json::from_value(a(0))?, super::api::opt(&a(1))?.as_ref()).await?;
            Value::Null
        }
        "abort" => {
            session.abort().await?;
            Value::Null
        }
        "waitForIdle" => {
            session.wait_for_idle().await?;
            Value::Null
        }
        "setModel" => {
            let options: Option<ModelMutationOptions> = super::api::opt(&a(1))?;
            session.set_model(serde_json::from_value(a(0))?, options.as_ref()).await?;
            Value::Null
        }
        "cycleModel" => {
            let options: Option<ModelMutationOptions> = super::api::opt(&a(1))?;
            serde_json::to_value(session.cycle_model(super::api::opt(&a(0))?, options.as_ref()).await?)?
        }
        "compact" => serde_json::to_value(session.compact(a(0).as_str()).await?)?,
        "bindExtensions" => {
            let o = a(0);
            let on_error = bridge.handle(&o["onError"]).map(|h| {
                Arc::new(move |error: crate::core::extensions::types::ExtensionError| {
                    super::api::post_call(&h, None, vec![serde_json::to_value(&error).unwrap_or(Value::Null)]);
                }) as crate::core::extensions::runner::ExtensionErrorListener
            });
            let shutdown_handler = bridge.handle(&o["shutdownHandler"]).map(|h| {
                Arc::new(move || super::api::post_call(&h, None, vec![])) as crate::core::extensions::runner::ShutdownHandler
            });
            let abort_handler = bridge
                .handle(&o["abortHandler"])
                .map(|h| Arc::new(move || super::api::post_call(&h, None, vec![])) as Arc<dyn Fn() + Send + Sync>);
            let bindings = ExtensionBindings {
                ui_context: None,
                mode: super::api::opt(&o["mode"])?,
                command_context_actions: None,
                abort_handler,
                shutdown_handler,
                on_error,
            };
            session.bind_extensions(&bindings).await?;
            Value::Null
        }
        "reload" => {
            session.reload(None).await?;
            Value::Null
        }
        "navigateTree" => serde_json::to_value(session.navigate_tree(&s(0), super::api::opt(&a(1))?.as_ref()).await?)?,
        "exportToHtml" => json!(session.export_to_html(a(0).as_str(), super::api::opt(&a(1))?.as_ref()).await?),
        _ => return Err(anyhow::anyhow!("agentSession.{method} is not a function")),
    })
}

// ---------------------------------------------------------------------------
// session.agent.beforeToolCall / afterToolCall
// ---------------------------------------------------------------------------

use pi_agent::types::{
    AfterToolCallContext, AfterToolCallFn, AfterToolCallResult, AgentContext, BeforeToolCallContext, BeforeToolCallFn,
    BeforeToolCallResult,
};

/// A Rust tool-call hook handed to JavaScript (the prior hook a JS hook chains to).
#[derive(Clone)]
pub(crate) enum ToolHook {
    Before(BeforeToolCallFn),
    After(AfterToolCallFn),
}

/// The Rust context of a hook call in progress, so a JS hook that calls the prior Rust hook passes
/// it the original context (TS passes the same object along).
#[derive(Clone)]
pub(crate) enum HookContext {
    Before(Box<BeforeToolCallContext>),
    After(Box<AfterToolCallContext>),
}

fn agent_context_to_js(context: &AgentContext) -> Value {
    let mut out = json!({ "messages": serde_json::to_value(&context.messages).unwrap_or(Value::Null) });
    if let Some(tools) = &context.tools {
        out["tools"] = Value::Array(
            tools
                .iter()
                .map(|t| json!({ "name": t.name(), "label": t.label(), "description": t.description(), "parameters": t.parameters() }))
                .collect(),
        );
    }
    out
}

fn hook_context_to_js(context: &HookContext) -> Value {
    match context {
        HookContext::Before(c) => json!({
            "assistantMessage": serde_json::to_value(&c.assistant_message).unwrap_or(Value::Null),
            "toolCall": serde_json::to_value(&c.tool_call).unwrap_or(Value::Null),
            "args": c.args,
            "context": agent_context_to_js(&c.context),
        }),
        HookContext::After(c) => json!({
            "assistantMessage": serde_json::to_value(&c.assistant_message).unwrap_or(Value::Null),
            "toolCall": serde_json::to_value(&c.tool_call).unwrap_or(Value::Null),
            "args": c.args,
            "result": serde_json::to_value(&c.result).unwrap_or(Value::Null),
            "isError": c.is_error,
            "context": agent_context_to_js(&c.context),
        }),
    }
}

fn before_result_from_js(value: &Value, args: Option<Value>) -> Option<BeforeToolCallResult> {
    let result = &value["result"];
    if !result.is_object() && args.is_none() {
        return None;
    }
    Some(BeforeToolCallResult {
        block: result["block"].as_bool().unwrap_or(false),
        reason: result["reason"].as_str().map(str::to_string),
        terminate: result["terminate"].as_bool(),
        args,
    })
}

fn after_result_from_js(value: &Value) -> anyhow::Result<Option<AfterToolCallResult>> {
    let result = &value["result"];
    if !result.is_object() {
        return Ok(None);
    }
    Ok(Some(AfterToolCallResult {
        content: super::api::opt(&result["content"])?,
        details: result.get("details").filter(|v| !v.is_null()).cloned(),
        structured_content: result.get("structuredContent").filter(|v| !v.is_null()).cloned(),
        is_error: result["isError"].as_bool(),
        usage: super::api::opt(&result["usage"])?,
        terminate: result["terminate"].as_bool(),
    }))
}

/// Run a JS hook with the Rust context; returns the glue result `{ result, args }`.
async fn run_js_hook(
    bridge: &Arc<Bridge>,
    handle: &pi_jsrt::JsHandle,
    context: HookContext,
    signal: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<Value> {
    let js_context = hook_context_to_js(&context);
    let context_id = bridge.hook_contexts.insert(context);
    let (signal_value, _guard) = bridge.signal_for(signal);
    let result = bridge
        .glue_call("invokeToolHook", vec![handle.to_value(), js_context, json!(context_id), signal_value])
        .await;
    bridge.hook_contexts.remove(context_id);
    result
}

fn js_before_hook(bridge: &Arc<Bridge>, handle: pi_jsrt::JsHandle) -> BeforeToolCallFn {
    let bridge = bridge.clone();
    Arc::new(move |context: BeforeToolCallContext, signal| {
        let bridge = bridge.clone();
        let handle = handle.clone();
        Box::pin(async move {
            let original_args = context.args.clone();
            let value = run_js_hook(&bridge, &handle, HookContext::Before(Box::new(context)), &signal).await?;
            let args = value.get("args").filter(|a| **a != original_args).cloned();
            Ok(before_result_from_js(&value, args))
        })
    })
}

fn js_after_hook(bridge: &Arc<Bridge>, handle: pi_jsrt::JsHandle) -> AfterToolCallFn {
    let bridge = bridge.clone();
    Arc::new(move |context: AfterToolCallContext, signal| {
        let bridge = bridge.clone();
        let handle = handle.clone();
        Box::pin(async move {
            let value = run_js_hook(&bridge, &handle, HookContext::After(Box::new(context)), &signal).await?;
            after_result_from_js(&value)
        })
    })
}

/// `session.agent.beforeToolCall` / `afterToolCall` reads and writes (`agent.*` session methods).
fn agent_hook_call(bridge: &Arc<Bridge>, session: &Arc<AgentSession>, session_id: u64, method: &str, arg0: &Value) -> anyhow::Result<Value> {
    let agent = session.agent();
    let describe = |object: Option<Value>, hook: ToolHook| match object {
        Some(origin) => json!({ "object": origin }),
        None => json!({ "native": bridge.tool_hooks.insert(hook), "session": session_id }),
    };
    Ok(match method {
        "agent.getBeforeToolCall" => match agent.before_tool_call() {
            Some(f) => describe(bridge.origins.get(&f), ToolHook::Before(f)),
            None => Value::Null,
        },
        "agent.getAfterToolCall" => match agent.after_tool_call() {
            Some(f) => describe(bridge.origins.get(&f), ToolHook::After(f)),
            None => Value::Null,
        },
        "agent.setBeforeToolCall" => {
            let hook = bridge.handle(arg0).map(|h| {
                let f = js_before_hook(bridge, h.clone());
                bridge.origins.record(&f, h);
                f
            });
            agent.set_before_tool_call(hook);
            Value::Null
        }
        "agent.setAfterToolCall" => {
            let hook = bridge.handle(arg0).map(|h| {
                let f = js_after_hook(bridge, h.clone());
                bridge.origins.record(&f, h);
                f
            });
            agent.set_after_tool_call(hook);
            Value::Null
        }
        _ => return Err(anyhow::anyhow!("agentSession.{method} is not a function")),
    })
}

/// A JS hook calling a Rust hook (`agent.callHook`): with the original Rust context when the JS
/// context came from Rust, else one built from the JS object.
async fn call_rust_hook(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let hook = bridge
        .tool_hooks
        .get(arg(payload, 0).as_u64().unwrap_or_default())
        .ok_or_else(|| anyhow::anyhow!("This tool-call hook is no longer available"))?;
    let js = arg(payload, 1);
    let stored = js["$hookContext"].as_u64().and_then(|id| bridge.hook_contexts.get(id));
    let signal = bridge.token(&payload["signal"]).unwrap_or_default();
    let agent_context = |value: &Value| AgentContext {
        messages: serde_json::from_value(value["messages"].clone()).unwrap_or_default(),
        tools: None,
    };
    match hook {
        ToolHook::Before(f) => {
            let mut context = match stored {
                Some(HookContext::Before(c)) => *c,
                _ => BeforeToolCallContext {
                    assistant_message: serde_json::from_value(js["assistantMessage"].clone())?,
                    tool_call: serde_json::from_value(js["toolCall"].clone())?,
                    args: js["args"].clone(),
                    context: agent_context(&js["context"]),
                },
            };
            if !js["args"].is_null() {
                context.args = js["args"].clone();
            }
            Ok(match f(context, signal).await? {
                Some(r) => {
                    let mut out = json!({ "block": r.block });
                    if let Some(reason) = r.reason {
                        out["reason"] = json!(reason);
                    }
                    if let Some(t) = r.terminate {
                        out["terminate"] = json!(t);
                    }
                    out
                }
                None => Value::Null,
            })
        }
        ToolHook::After(f) => {
            let context = match stored {
                Some(HookContext::After(c)) => *c,
                _ => AfterToolCallContext {
                    assistant_message: serde_json::from_value(js["assistantMessage"].clone())?,
                    tool_call: serde_json::from_value(js["toolCall"].clone())?,
                    args: js["args"].clone(),
                    result: serde_json::from_value(js["result"].clone())?,
                    is_error: js["isError"].as_bool().unwrap_or(false),
                    context: agent_context(&js["context"]),
                },
            };
            Ok(match f(context, signal).await? {
                Some(r) => {
                    let mut out = serde_json::Map::new();
                    if let Some(c) = r.content {
                        out.insert("content".into(), serde_json::to_value(c)?);
                    }
                    if let Some(d) = r.details {
                        out.insert("details".into(), d);
                    }
                    if let Some(s) = r.structured_content {
                        out.insert("structuredContent".into(), s);
                    }
                    if let Some(e) = r.is_error {
                        out.insert("isError".into(), json!(e));
                    }
                    if let Some(u) = r.usage {
                        out.insert("usage".into(), serde_json::to_value(u)?);
                    }
                    if let Some(t) = r.terminate {
                        out.insert("terminate".into(), json!(t));
                    }
                    Value::Object(out)
                }
                None => Value::Null,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Loaded extensions and extension runtimes handed to JavaScript
// ---------------------------------------------------------------------------

use crate::core::extensions::types::{ExtensionRef, ExtensionRuntime};

/// The JS id of a loaded extension (one per extension, held weakly).
fn extension_id(bridge: &Bridge, extension: &ExtensionRef) -> u64 {
    let key = Arc::as_ptr(extension) as *const () as usize;
    let mut ids = bridge.extension_ids.lock();
    if let Some(id) = ids.get(&key)
        && bridge.extension_refs.get(*id).and_then(|w| w.upgrade()).is_some_and(|e| Arc::ptr_eq(&e, extension))
    {
        return *id;
    }
    ids.retain(|_, id| match bridge.extension_refs.get(*id) {
        Some(w) if w.strong_count() > 0 => true,
        _ => {
            bridge.extension_refs.remove(*id);
            false
        }
    });
    let id = bridge.extension_refs.insert(Arc::downgrade(extension));
    ids.insert(key, id);
    id
}

/// The JS id of an extension runtime (`createExtensionRuntime()`, `LoadExtensionsResult.runtime`).
fn ext_runtime_id(bridge: &Bridge, runtime: &Arc<ExtensionRuntime>) -> u64 {
    let key = Arc::as_ptr(runtime) as usize;
    let mut ids = bridge.ext_runtime_ids.lock();
    if let Some(id) = ids.get(&key)
        && bridge.ext_runtimes.get(*id).is_some_and(|r| Arc::ptr_eq(&r, runtime))
    {
        return *id;
    }
    let id = bridge.ext_runtimes.insert(runtime.clone());
    ids.insert(key, id);
    id
}

/// A `LoadExtensionsResult` from JavaScript (a custom ResourceLoader's `getExtensions()`): its
/// extensions must be ones pi loaded (they carry their Rust identity).
fn extensions_result_from_js(bridge: &Bridge, value: &Value, fallback_runtime: &Arc<ExtensionRuntime>) -> anyhow::Result<LoadExtensionsResult> {
    let mut extensions = Vec::new();
    for e in value["extensions"].as_array().into_iter().flatten() {
        let extension = e["$extension"]
            .as_u64()
            .and_then(|id| bridge.extension_refs.get(id))
            .and_then(|w| w.upgrade())
            .ok_or_else(|| anyhow::anyhow!("pi-rs: a ResourceLoader can only return extensions loaded by pi (DefaultResourceLoader, discoverAndLoadExtensions)"))?;
        extensions.push(extension);
    }
    let runtime = value["$runtime"].as_u64().and_then(|id| bridge.ext_runtimes.get(id)).unwrap_or_else(|| fallback_runtime.clone());
    Ok(LoadExtensionsResult {
        extensions,
        errors: super::api::opt(&value["errors"])?.unwrap_or_default(),
        warnings: super::api::opt(&value["warnings"])?,
        runtime,
    })
}

/// A ResourceLoader implemented in JavaScript (an object with `getExtensions()`, `getSkills()`, ...).
pub(crate) struct JsResourceLoader {
    bridge: std::sync::Weak<Bridge>,
    handle: pi_jsrt::JsHandle,
    runtime: Arc<ExtensionRuntime>,
}

impl JsResourceLoader {
    fn call(&self, method: &str, args: Vec<Value>) -> Option<Value> {
        let bridge = self.bridge.upgrade()?;
        bridge.glue_call_blocking("invokeLoaderMethod", vec![self.handle.to_value(), json!(method), Value::Array(args)]).ok()
    }

    fn call_as<T: serde::de::DeserializeOwned + Default>(&self, method: &str) -> T {
        self.call(method, vec![]).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl ResourceLoader for JsResourceLoader {
    fn get_extensions(&self) -> LoadExtensionsResult {
        let empty = || LoadExtensionsResult { extensions: Vec::new(), errors: Vec::new(), warnings: None, runtime: self.runtime.clone() };
        let (Some(bridge), Some(value)) = (self.bridge.upgrade(), self.call("getExtensions", vec![])) else { return empty() };
        extensions_result_from_js(&bridge, &value, &self.runtime).unwrap_or_else(|e| {
            LoadExtensionsResult {
                extensions: Vec::new(),
                errors: vec![crate::core::extensions::types::ExtensionLoadError { path: "<resource loader>".into(), error: e.to_string() }],
                warnings: None,
                runtime: self.runtime.clone(),
            }
        })
    }
    fn get_skills(&self) -> crate::core::resource_loader::ResourceLoaderGetSkillsResult {
        self.call_as("getSkills")
    }
    fn get_prompts(&self) -> crate::core::resource_loader::ResourceLoaderGetPromptsResult {
        self.call_as("getPrompts")
    }
    fn get_themes(&self) -> crate::core::resource_loader::ResourceLoaderGetThemesResult {
        // Theme objects stay in JavaScript; a JS loader contributes no themes to pi.
        Default::default()
    }
    fn get_agents_files(&self) -> crate::core::resource_loader::ResourceLoaderGetAgentsFilesResult {
        self.call_as("getAgentsFiles")
    }
    fn get_system_prompt(&self) -> Option<String> {
        self.call("getSystemPrompt", vec![]).and_then(|v| v.as_str().map(str::to_string))
    }
    fn get_system_prompt_source(&self) -> Option<crate::core::resource_loader::ResourceLoaderGetSystemPromptSourceResult> {
        self.call("getSystemPromptSource", vec![]).and_then(|v| serde_json::from_value(v).ok())
    }
    fn get_append_system_prompt(&self) -> Vec<String> {
        self.call_as("getAppendSystemPrompt")
    }
    fn get_append_system_prompt_sources(&self) -> Vec<crate::core::resource_loader::ResourceLoaderGetAppendSystemPromptSourcesResult> {
        self.call_as("getAppendSystemPromptSources")
    }
    fn extend_resources(&self, paths: &crate::core::resource_loader::ResourceExtensionPaths) {
        self.call("extendResources", vec![serde_json::to_value(paths).unwrap_or(Value::Null)]);
    }
    async fn reload(&self, _options: Option<&crate::core::resource_loader::ResourceLoaderReloadOptions>) -> anyhow::Result<()> {
        let bridge = self.bridge.upgrade().ok_or_else(|| anyhow::anyhow!("extension runtime stopped"))?;
        bridge.glue_call("invokeLoaderReload", vec![self.handle.to_value()]).await?;
        Ok(())
    }
}

pub(crate) fn js_resource_loader(bridge: &Arc<Bridge>, value: &Value) -> anyhow::Result<Arc<dyn ResourceLoader>> {
    if let Some(id) = value["$loader"].as_u64() {
        return Ok(bridge.resource_loaders.get(id).ok_or_else(|| anyhow::anyhow!("unknown resource loader"))? as Arc<dyn ResourceLoader>);
    }
    let handle = bridge.handle(&value["object"]).ok_or_else(|| anyhow::anyhow!("resourceLoader must be a ResourceLoader object"))?;
    Ok(Arc::new(JsResourceLoader {
        bridge: Arc::downgrade(bridge),
        handle,
        runtime: crate::core::extensions::loader::create_extension_runtime(),
    }))
}

// ---------------------------------------------------------------------------
// ProjectTrustStore, DefaultPackageManager, model resolution, extension loading helpers
// ---------------------------------------------------------------------------

use crate::core::package_manager::{DefaultPackageManager, PackageManager, PackageManagerOptions};
use crate::core::trust_manager::ProjectTrustStore;

fn misc_call(bridge: &Arc<Bridge>, op: &str, payload: &Value) -> anyhow::Result<Value> {
    let m = str_arg(payload, "m");
    let a = |i: usize| arg(payload, i);
    let s = |i: usize| arg(payload, i).as_str().unwrap_or_default().to_string();
    match op {
        "trust" => {
            let store = ProjectTrustStore::new(payload["agentDir"].as_str().unwrap_or_default());
            Ok(match m.as_str() {
                "get" => json!(store.get(&s(0))?),
                "getEntry" => serde_json::to_value(store.get_entry(&s(0))?)?,
                "set" => {
                    store.set(&s(0), super::api::opt(&a(1))?)?;
                    Value::Null
                }
                "setMany" => {
                    let updates: Vec<Value> = serde_json::from_value(a(0))?;
                    let updates: Vec<crate::core::trust_manager::ProjectTrustUpdate> = updates
                        .iter()
                        .map(|u| crate::core::trust_manager::ProjectTrustUpdate {
                            path: str_arg(u, "path"),
                            decision: u["decision"].as_bool(),
                        })
                        .collect();
                    store.set_many(&updates)?;
                    Value::Null
                }
                _ => return Err(anyhow::anyhow!("ProjectTrustStore.{m} is not a function")),
            })
        }
        "pm" => {
            if m == "new" {
                let o = a(0);
                let settings = match o["settingsManager"].as_u64() {
                    Some(id) => bridge.settings_managers.get(id).ok_or_else(|| anyhow::anyhow!("unknown settings manager"))?,
                    None => anyhow::bail!("DefaultPackageManager requires a settingsManager"),
                };
                let manager = DefaultPackageManager::new(&PackageManagerOptions {
                    cwd: o["cwd"].as_str().map(str::to_string).unwrap_or_else(|| bridge.cwd.clone()),
                    agent_dir: o["agentDir"].as_str().map(str::to_string).unwrap_or_else(crate::config::get_agent_dir),
                    settings_manager: settings,
                    builtin_extensions: super::api::opt(&o["builtinExtensions"])?,
                });
                return Ok(json!(bridge.package_managers.insert(Arc::new(manager))));
            }
            if m == "release" {
                bridge.package_managers.remove(payload["id"].as_u64().unwrap_or_default());
                return Ok(Value::Null);
            }
            let pm = bridge.package_managers.get(payload["id"].as_u64().unwrap_or_default()).ok_or_else(|| anyhow::anyhow!("unknown package manager"))?;
            Ok(match m.as_str() {
                "listConfiguredPackages" => serde_json::to_value(pm.list_configured_packages())?,
                "addSourceToSettings" => json!(pm.add_source_to_settings(&s(0), super::api::opt(&a(1))?.as_ref())?),
                "removeSourceFromSettings" => json!(pm.remove_source_from_settings(&s(0), super::api::opt(&a(1))?.as_ref())?),
                "getInstalledPath" => json!(pm.get_installed_path(&s(0), serde_json::from_value(a(1))?)?),
                "setProgressCallback" => {
                    let callback = bridge.handle(&a(0)).map(|h| {
                        Arc::new(move |event: crate::core::package_manager::ProgressEvent| {
                            super::api::post_call(&h, None, vec![serde_json::to_value(&event).unwrap_or(Value::Null)]);
                        }) as crate::core::package_manager::ProgressCallback
                    });
                    pm.set_progress_callback(callback.as_ref());
                    Value::Null
                }
                _ => return Err(anyhow::anyhow!("packageManager.{m} is not a function")),
            })
        }
        "sdk.resolveCliModel" => {
            let o = a(0);
            let runtime = runtime_of(bridge, &json!({ "runtime": o["modelRuntime"] }))?;
            let result = crate::core::model_resolver::resolve_cli_model(&crate::core::model_resolver::ResolveCliModelOptions {
                cli_provider: o["cliProvider"].as_str().map(str::to_string),
                cli_model: o["cliModel"].as_str().map(str::to_string),
                cli_thinking: super::api::opt(&o["cliThinking"])?,
                inline_api_key: o["inlineApiKey"].as_bool(),
                model_runtime: runtime,
            });
            Ok(serde_json::to_value(result)?)
        }
        "sdk.createExtensionRuntime" => Ok(json!(ext_runtime_id(bridge, &crate::core::extensions::loader::create_extension_runtime()))),
        // `extension.tools.get(name).definition` of a loaded extension: the registered definition.
        "sdk.extensionTool" => {
            let extension = bridge
                .extension_refs
                .get(payload["ext"].as_u64().unwrap_or_default())
                .and_then(|weak| weak.upgrade())
                .ok_or_else(|| anyhow::anyhow!("This extension is no longer loaded"))?;
            let definition = extension.read().tools.get(&s(0)).map(|tool| tool.definition.clone());
            Ok(definition.map(|d| super::builtin::tool_definition_to_js(bridge, d)).unwrap_or(Value::Null))
        }
        // `createAgentSessionServices`: the resolved cwd and agent dir, and the runtime's files.
        "sdk.servicesPaths" => {
            let cwd = crate::utils::paths::resolve_path(&s(0), None, None);
            let agent_dir = match a(1).as_str() {
                Some(dir) if !dir.is_empty() => crate::utils::paths::resolve_path(dir, None, None),
                _ => crate::config::get_agent_dir(),
            };
            let file = |name: &str| std::path::Path::new(&agent_dir).join(name).to_string_lossy().into_owned();
            Ok(json!({ "cwd": cwd, "agentDir": agent_dir, "authPath": file("auth.json"), "modelsPath": file("models.json") }))
        }
        _ => Err(anyhow::anyhow!("unknown host call {op}")),
    }
}

async fn misc_async(bridge: &Arc<Bridge>, op: &str, payload: &Value) -> anyhow::Result<Value> {
    let a = |i: usize| arg(payload, i);
    let s = |i: usize| arg(payload, i).as_str().unwrap_or_default().to_string();
    match op {
        "pm.async" => {
            let pm = bridge.package_managers.get(payload["id"].as_u64().unwrap_or_default()).ok_or_else(|| anyhow::anyhow!("unknown package manager"))?;
            let m = str_arg(payload, "m");
            Ok(match m.as_str() {
                "resolve" => {
                    let on_missing = bridge.handle(&a(0)).map(|h| {
                        let b = bridge.clone();
                        Arc::new(move |source: String| {
                            let b = b.clone();
                            let h = h.clone();
                            Box::pin(async move {
                                let value = b.glue_call("callFunctionAsync", vec![h.to_value(), json!([source])]).await?;
                                let action: crate::core::package_manager::MissingSourceAction = serde_json::from_value(value)?;
                                Ok(action)
                            }) as futures::future::BoxFuture<'static, anyhow::Result<crate::core::package_manager::MissingSourceAction>>
                        }) as crate::core::package_manager::MissingSourceHandler
                    });
                    serde_json::to_value(pm.resolve(on_missing).await?)?
                }
                "install" => {
                    pm.install(&s(0), super::api::opt(&a(1))?.as_ref()).await?;
                    Value::Null
                }
                "installAndPersist" => {
                    pm.install_and_persist(&s(0), super::api::opt(&a(1))?.as_ref()).await?;
                    Value::Null
                }
                "remove" => {
                    pm.remove(&s(0), super::api::opt(&a(1))?.as_ref()).await?;
                    Value::Null
                }
                "removeAndPersist" => json!(pm.remove_and_persist(&s(0), super::api::opt(&a(1))?.as_ref()).await?),
                "update" => {
                    pm.update(a(0).as_str()).await?;
                    Value::Null
                }
                "resolveExtensionSources" => {
                    let sources: Vec<String> = serde_json::from_value(a(0))?;
                    serde_json::to_value(pm.resolve_extension_sources(&sources, super::api::opt(&a(1))?.as_ref()).await?)?
                }
                "checkForAvailableUpdates" => serde_json::to_value(pm.check_for_available_updates().await?)?,
                _ => return Err(anyhow::anyhow!("packageManager.{m} is not a function")),
            })
        }
        "sdk.resolveModelScope" => {
            let runtime = runtime_of(bridge, &json!({ "runtime": a(1) }))?;
            let patterns: Vec<String> = serde_json::from_value(a(0))?;
            let options = auth_options(bridge, &a(2));
            let result = crate::core::model_resolver::resolve_model_scope_with_diagnostics(&patterns, runtime, Some(&options)).await?;
            Ok(json!({
                "scopedModels": serde_json::to_value(&result.scoped_models)?,
                "diagnostics": serde_json::to_value(&result.diagnostics)?,
            }))
        }
        // The rest of `createAgentSessionServices` once its loader has reloaded: register what the
        // extensions queued, refresh offline, apply extension flag values.
        "sdk.finishServices" => {
            use crate::core::agent_session_services::{apply_extension_flag_values, apply_pending_extension_registrations};
            let loader = loader_of(bridge, &json!({ "id": payload["loader"] }))?;
            let runtime = runtime_of(bridge, payload)?;
            let mut diagnostics = apply_pending_extension_registrations(loader.as_ref(), &runtime);
            runtime
                .refresh(Some(pi_ai::models::ModelsRefreshOptions { allow_network: Some(false), ..Default::default() }))
                .await?;
            let flags: Option<indexmap::IndexMap<String, crate::core::extensions::types::FlagValue>> = match &payload["flags"] {
                Value::Array(entries) => Some(
                    entries
                        .iter()
                        .filter_map(|e| Some((e[0].as_str()?.to_string(), serde_json::from_value(e[1].clone()).ok()?)))
                        .collect(),
                ),
                _ => None,
            };
            diagnostics.extend(apply_extension_flag_values(loader.as_ref(), flags.as_ref()));
            Ok(serde_json::to_value(diagnostics)?)
        }
        "sdk.discoverAndLoadExtensions" => {
            let paths: Vec<String> = serde_json::from_value(a(0))?;
            let (cwd, agent_dir) = (s(1), a(2).as_str().map(str::to_string));
            // Loading extensions may start a JS runtime whose host functions run this very
            // dispatcher: drive it on a blocking thread so this future's type does not contain it.
            let handle = tokio::runtime::Handle::current();
            let result = tokio::task::spawn_blocking(move || {
                handle.block_on(crate::core::extensions::loader::discover_and_load_extensions(
                    &paths,
                    &cwd,
                    agent_dir.as_deref(),
                    None,
                ))
            })
            .await??;
            Ok(extensions_result_to_js(bridge, &result))
        }
        _ => Err(anyhow::anyhow!("unknown async host call {op}")),
    }
}
