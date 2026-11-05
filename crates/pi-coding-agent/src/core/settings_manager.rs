//! Port of `packages/coding-agent/src/core/settings-manager.ts`.
//!
//! Settings are held as JSON objects (like the TS `Settings` records, which are never validated):
//! unknown keys survive, invalid values only matter to the getter that reads them, and files are
//! rewritten with `JSON.stringify(settings, null, 2)` formatting. [`SettingsManager::get_settings`]
//! converts the effective JSON to the typed [`Settings`] struct, dropping fields whose value does
//! not fit the schema.
//!
//! Writes happen synchronously when a setter runs (serialized by a write lock), which is what the
//! TS write queue amounts to; [`SettingsManager::flush`] is therefore a no-op kept for API parity.

use crate::compat::WheelScrollLines;
use crate::config::{CONFIG_DIR_NAME, get_agent_dir};
use crate::core::settings_defaults as defaults;
use crate::modes::interactive::components::tree_selector::FilterMode;
use crate::utils::lockfile::{LockError, LockOptions, lock_sync_with_retry};
use crate::utils::paths::{node_dirname, node_join, normalize_path, resolve_path};
use crate::utils::text::strip_bom;
use indexmap::{IndexMap, IndexSet};
use parking_lot::Mutex;
use pi_agent::types::ThinkingLevel;
use crate::core::settings_schema::open_string_enum;
use pi_ai::types::ProviderId;
use pi_tui::components::scroll_view::ScrollViewScrollbar;
use pi_tui::terminal_image::{CapabilityOverrides, ImageProtocol};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;

pub use crate::core::settings_schema::BranchSummarySettings;
pub use crate::core::settings_schema::CacheWarmingMode;
pub use crate::core::settings_schema::CodemodeMode;
pub use crate::core::settings_schema::CodemodeSettings;
pub use crate::core::settings_schema::CompactionModelOverride;
pub use crate::core::settings_schema::CompactionSettings;
pub use crate::core::settings_schema::DefaultProjectTrust;
pub use crate::core::settings_schema::FullscreenExitOutput;
pub use crate::core::settings_schema::ImageSettings;
pub use crate::core::settings_schema::MarkdownSettings;
pub use crate::core::settings_schema::MermaidRenderingMode;
pub use crate::core::settings_schema::PackageSource;
pub use crate::core::settings_schema::ProviderRetrySettings;
pub use crate::core::settings_schema::QuietStartup;
pub use crate::core::settings_schema::RetrySettings;
pub use crate::core::settings_schema::SessionShareDestination;
pub use crate::core::settings_schema::Settings;
pub use crate::core::settings_schema::TerminalSettings;
pub use crate::core::settings_schema::ThinkingBudgetsSettings;
pub use crate::core::settings_schema::TransportSetting;
pub use crate::core::settings_schema::TuiMode;
pub use crate::core::settings_schema::WarningSettings;

/// A settings JSON object.
pub type JsonMap = Map<String, Value>;

/// Cache-warming profile. "idle" also warms between agent runs.
pub const CACHE_WARMING_MODES: &[&str] = &["off", "streaming", "idle"];

/// Tools enabled at startup when `defaultTools` does not change them.
pub const DEFAULT_TOOL_NAMES: &[&str] = &["read", "bash", "edit", "write"];

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

// ============================================================================
// JS value helpers
// ============================================================================

/// `String(value)` for a JSON value.
pub(crate) fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_number_string(n),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

fn js_number_string(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    js_f64_string(n.as_f64().unwrap_or(f64::NAN))
}

pub(crate) fn js_f64_string(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_string()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else if f.fract() == 0.0 && f.abs() < 1e21 {
        format!("{}", f as i128)
    } else {
        format!("{f}")
    }
}

fn is_mergeable_object(value: &Value) -> bool {
    value.is_object()
}

/// JS truthiness of a JSON value (`undefined` is `None`).
fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `obj.key ?? undefined`: the value unless it is missing or null.
fn present<'a>(map: &'a JsonMap, key: &str) -> Option<&'a Value> {
    map.get(key).filter(|v| !v.is_null())
}

/// `obj.key?.sub` (raw, may be null).
fn nested_raw<'a>(map: &'a JsonMap, key: &str, sub: &str) -> Option<&'a Value> {
    map.get(key).and_then(Value::as_object).and_then(|o| o.get(sub))
}

/// `obj.key?.sub ?? undefined`.
fn nested<'a>(map: &'a JsonMap, key: &str, sub: &str) -> Option<&'a Value> {
    nested_raw(map, key, sub).filter(|v| !v.is_null())
}

fn finite_number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|f| f.is_finite())
}

fn number_i64(value: Option<&Value>, default: i64) -> i64 {
    match value.and_then(Value::as_f64) {
        Some(f) if f.is_finite() => f as i64,
        _ => default,
    }
}

fn number_u64(value: Option<&Value>, default: u64) -> u64 {
    match value.and_then(Value::as_f64) {
        Some(f) if f.is_finite() && f >= 0.0 => f as u64,
        _ => default,
    }
}

/// `value ?? default` read as a boolean: settings files are not validated, and callers test the
/// value for truthiness.
fn bool_or(value: Option<&Value>, default: bool) -> bool {
    match value {
        None | Some(Value::Null) => default,
        Some(value) => is_truthy(Some(value)),
    }
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn to_json<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Convert a settings JSON object to the typed [`Settings`], dropping fields whose values do not
/// match the schema.
pub fn settings_from_json(map: &JsonMap) -> Settings {
    if let Ok(settings) = serde_json::from_value::<Settings>(Value::Object(map.clone())) {
        return settings;
    }
    let mut valid = JsonMap::new();
    for (key, value) in map {
        let mut single = JsonMap::new();
        single.insert(key.clone(), value.clone());
        if serde_json::from_value::<Settings>(Value::Object(single)).is_ok() {
            valid.insert(key.clone(), value.clone());
        }
    }
    serde_json::from_value(Value::Object(valid)).unwrap_or_default()
}

/// Convert typed [`Settings`] to a JSON object (unset fields are omitted, like `undefined`).
pub fn settings_to_json(settings: &Settings) -> JsonMap {
    match serde_json::to_value(settings) {
        Ok(Value::Object(map)) => map,
        _ => JsonMap::new(),
    }
}

fn deep_merge_objects(base: &JsonMap, overrides: &JsonMap) -> JsonMap {
    let mut result = base.clone();
    for (key, override_value) in overrides {
        let merged = match (base.get(key), override_value) {
            (Some(Value::Object(base_obj)), Value::Object(override_obj)) => {
                Value::Object(deep_merge_objects(base_obj, override_obj))
            }
            _ => override_value.clone(),
        };
        result.insert(key.clone(), merged);
    }
    result
}

/// Whether a tool selection entry is a `+name` or `-name` modifier.
pub fn is_tool_modifier(entry: &Value) -> bool {
    entry.as_str().is_some_and(is_tool_modifier_str)
}

/// [`is_tool_modifier`] for a string entry.
pub fn is_tool_modifier_str(entry: &str) -> bool {
    entry.starts_with('+') || entry.starts_with('-')
}

/// Validate a tool list from `--tools` or the SDK `tools` option. It is either an allowlist of plain
/// names and patterns or a list of only `+name`/`-name` entries with exact names. Returns the
/// problem, or undefined when the list is valid.
pub fn get_tool_list_error(entries: &[String]) -> Option<String> {
    let modifiers: Vec<&String> = entries.iter().filter(|e| is_tool_modifier_str(e)).collect();
    if modifiers.is_empty() {
        return None;
    }
    if modifiers.len() < entries.len() {
        return Some("tool names cannot be mixed with +name or -name entries".to_string());
    }
    if let Some(pattern) = modifiers.iter().find(|e| e.contains('*')) {
        return Some(format!(
            "+name and -name entries take exact tool names, not patterns: {pattern}"
        ));
    }
    None
}

/// Apply the `+name` and `-name` entries of `entries` to `base` in order: `+name` adds a tool and
/// `-name` removes one. Other entries are ignored.
pub fn apply_tool_modifiers(base: &[String], entries: &[String]) -> Vec<String> {
    let mut tools: Vec<String> = base.to_vec();
    for entry in entries {
        if !is_tool_modifier_str(entry) {
            continue;
        }
        let name = &entry[1..];
        let index = tools.iter().position(|t| t == name);
        if entry.starts_with('+') && index.is_none() && !name.is_empty() {
            tools.push(name.to_string());
        } else if entry.starts_with('-')
            && let Some(index) = index
        {
            tools.remove(index);
        }
    }
    tools
}

/// Merge `defaultTools` of two settings layers. A list with plain tool names replaces the inherited
/// one; a list of only `+name`/`-name` entries is appended, so it modifies the inherited selection.
fn merge_default_tools(base: Option<&Value>, overrides: Option<&Value>) -> Option<Value> {
    let Some(overrides) = overrides else {
        return base.cloned();
    };
    // Settings files are not validated; a malformed value replaces instead of throwing here.
    match (base, overrides) {
        (Some(Value::Array(base_items)), Value::Array(override_items))
            if override_items.iter().all(is_tool_modifier) =>
        {
            let mut merged = base_items.clone();
            merged.extend(override_items.iter().cloned());
            Some(Value::Array(merged))
        }
        _ => Some(overrides.clone()),
    }
}

/// Resolve a merged `defaultTools` list: plain names replace `DEFAULT_TOOL_NAMES`, then `+name` adds
/// and `-name` removes a tool, in list order.
fn resolve_default_tools(entries: &[String]) -> Vec<String> {
    let plain: Vec<String> = entries
        .iter()
        .filter(|e| !is_tool_modifier_str(e))
        .cloned()
        .collect();
    let base: Vec<String> = if !plain.is_empty() || entries.is_empty() {
        plain
    } else {
        DEFAULT_TOOL_NAMES.iter().map(|s| s.to_string()).collect()
    };
    apply_tool_modifiers(&base, entries)
}

/// Deep merge settings: project/overrides take precedence, nested objects merge recursively
fn deep_merge_settings(base: &JsonMap, overrides: &JsonMap) -> JsonMap {
    let mut merged = deep_merge_objects(base, overrides);
    if let Some(default_tools) =
        merge_default_tools(base.get("defaultTools"), overrides.get("defaultTools"))
    {
        merged.insert("defaultTools".to_string(), default_tools);
    }
    merged
}

fn parse_timeout_setting(value: Option<&Value>, setting_name: &str) -> anyhow::Result<Option<i64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if let Some(timeout_ms) = crate::core::http_dispatcher::parse_http_idle_timeout_ms(value.clone()) {
        return Ok(Some(timeout_ms));
    }
    anyhow::bail!("Invalid {setting_name} setting: {}", js_string(value))
}

// ============================================================================
// Storage
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SettingsScope {
    #[serde(rename = "global")]
    Global,
    #[serde(rename = "project")]
    Project,
}

impl SettingsScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsManagerCreateOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_trusted: Option<bool>,
}

/// Callback run under the storage lock: receives the current file content and returns the new
/// content to write (`None` leaves the file untouched).
pub type SettingsLockFn<'a> =
    &'a mut dyn FnMut(Option<String>) -> anyhow::Result<Option<String>>;

pub trait SettingsStorage: Send + Sync {
    fn with_lock(&self, scope: SettingsScope, f: SettingsLockFn<'_>) -> anyhow::Result<()>;
}

#[derive(Debug, Clone)]
pub struct SettingsError {
    pub scope: SettingsScope,
    pub path: Option<String>,
    pub error: Arc<anyhow::Error>,
}

pub type SettingsPaths = IndexMap<SettingsScope, String>;

fn to_settings_error(
    scope: SettingsScope,
    error: Arc<anyhow::Error>,
    path: Option<&String>,
) -> SettingsError {
    SettingsError {
        scope,
        path: path.filter(|p| !p.is_empty()).cloned(),
        error,
    }
}

/// A JavaScript error with its constructor name (`SyntaxError` from `JSON.parse`, `TypeError`),
/// for output that shows `error.stack`, whose first line is `${name}: ${message}`.
#[derive(Debug, Clone)]
pub struct JsError {
    pub name: &'static str,
    pub message: String,
}

impl JsError {
    pub fn new(name: &'static str, message: impl Into<String>) -> Self {
        Self { name, message: message.into() }
    }
}

impl std::fmt::Display for JsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JsError {}

/// The first line of the JavaScript `error.stack` for a settings error: `${error.name}: ${error.message}`
/// (`Error` for filesystem and lock errors).
pub fn js_error_stack_header(error: &anyhow::Error) -> String {
    let name = error.downcast_ref::<JsError>().map(|e| e.name).unwrap_or("Error");
    format!("{name}: {error}")
}

/// Node `fs` error for a failed settings file operation (`CODE: description, syscall 'path'`).
fn settings_fs_error(error: std::io::Error, syscall: &str, path: &str) -> anyhow::Error {
    use crate::core::tools::node_fs::node_fs_error;
    // Node reports reading a directory from the `read` syscall without a path.
    let fs_error = if error.kind() == std::io::ErrorKind::IsADirectory && syscall == "open" {
        node_fs_error(&error, "read", None)
    } else {
        node_fs_error(&error, syscall, Some(path))
    };
    anyhow::Error::new(fs_error)
}

/// TS: `implements SettingsStorage`
pub struct FileSettingsStorage {
    global_settings_path: String,
    project_settings_path: String,
}

impl FileSettingsStorage {
    pub fn new(cwd: &str, agent_dir: &str) -> Self {
        let resolved_cwd = resolve_path(cwd, None, None);
        let resolved_agent_dir = resolve_path(agent_dir, None, None);
        Self {
            global_settings_path: node_join(&[&resolved_agent_dir, "settings.json"]),
            project_settings_path: node_join(&[&resolved_cwd, CONFIG_DIR_NAME, "settings.json"]),
        }
    }

    fn acquire_lock_sync_with_retry(path: &str) -> anyhow::Result<crate::utils::lockfile::LockGuard> {
        lock_sync_with_retry(path, &LockOptions::default(), 10, Duration::from_millis(20)).map_err(
            |e| match e {
                LockError::Locked => anyhow::anyhow!("Lock file is already being held"),
                LockError::Io(e) => anyhow::Error::new(e),
            },
        )
    }
}

impl SettingsStorage for FileSettingsStorage {
    fn with_lock(&self, scope: SettingsScope, f: SettingsLockFn<'_>) -> anyhow::Result<()> {
        let path = match scope {
            SettingsScope::Global => &self.global_settings_path,
            SettingsScope::Project => &self.project_settings_path,
        };
        let dir = node_dirname(path);

        let mut release = None;
        // Only create directory and lock if file exists or we need to write
        let file_exists = std::path::Path::new(path).exists();
        if file_exists {
            release = Some(Self::acquire_lock_sync_with_retry(path)?);
        }
        // `readFileSync(path, "utf-8")` decodes invalid UTF-8 as U+FFFD.
        let current = if file_exists {
            let bytes = std::fs::read(path).map_err(|e| settings_fs_error(e, "open", path))?;
            Some(String::from_utf8_lossy(&bytes).into_owned())
        } else {
            None
        };
        let next = f(current)?;
        if let Some(next) = next {
            // Only create directory when we actually need to write
            if !std::path::Path::new(&dir).exists() {
                std::fs::create_dir_all(&dir).map_err(|e| settings_fs_error(e, "mkdir", &dir))?;
            }
            if release.is_none() {
                release = Some(Self::acquire_lock_sync_with_retry(path)?);
            }
            std::fs::write(path, next).map_err(|e| settings_fs_error(e, "open", path))?;
        }
        drop(release);
        Ok(())
    }
}

/// TS: `implements SettingsStorage`
#[derive(Default)]
pub struct InMemorySettingsStorage {
    global: Mutex<Option<String>>,
    project: Mutex<Option<String>>,
}

impl InMemorySettingsStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SettingsStorage for InMemorySettingsStorage {
    fn with_lock(&self, scope: SettingsScope, f: SettingsLockFn<'_>) -> anyhow::Result<()> {
        let slot = match scope {
            SettingsScope::Global => &self.global,
            SettingsScope::Project => &self.project,
        };
        let current = slot.lock().clone();
        if let Some(next) = f(current)? {
            *slot.lock() = Some(next);
        }
        Ok(())
    }
}

// ============================================================================
// Result types
// ============================================================================

/// `"all" | "one-at-a-time"`; the getters hand back unrecognized values verbatim like TS
/// (`this.settings.steeringMode || default`).
pub type SettingsManagerGetSteeringModeResult = pi_agent::types::QueueMode;

open_string_enum! {
    /// A thinking level as stored in settings. TS returns the stored value unchecked
    /// (`getDefaultThinkingLevel()`), so values outside `ThinkingLevel` are kept in `Other`.
    pub enum ThinkingLevelSetting {
        Off => "off",
        Minimal => "minimal",
        Low => "low",
        Medium => "medium",
        High => "high",
        Xhigh => "xhigh",
        Max => "max",
    }
}

impl ThinkingLevelSetting {
    /// The level to hand to the session. Every TS consumer clamps the level to the model
    /// (`clampThinkingLevel`), which maps an unknown level to the model's lowest supported level,
    /// exactly what clamping `"off"` yields; so unknown values become `Off`.
    pub fn effective_level(&self) -> ThinkingLevel {
        match self {
            Self::Off | Self::Other(_) => ThinkingLevel::Off,
            Self::Minimal => ThinkingLevel::Minimal,
            Self::Low => ThinkingLevel::Low,
            Self::Medium => ThinkingLevel::Medium,
            Self::High => ThinkingLevel::High,
            Self::Xhigh => ThinkingLevel::Xhigh,
            Self::Max => ThinkingLevel::Max,
        }
    }

    /// The known level, or `None` for a value outside `ThinkingLevel`.
    pub fn known_level(&self) -> Option<ThinkingLevel> {
        match self {
            Self::Other(_) => None,
            known => Some(known.effective_level()),
        }
    }

    /// JS truthiness of the stored value (`if (perModel)` in TS).
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Other(value) => is_truthy(Some(value)),
            _ => true,
        }
    }
}

impl From<ThinkingLevel> for ThinkingLevelSetting {
    fn from(level: ThinkingLevel) -> Self {
        Self::from_value(to_json(&level))
    }
}

/// `Pick<Model, "provider" | "id">` used to look up compaction model overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsManagerGetCompactionSettingsModel {
    pub provider: ProviderId,
    pub id: String,
}

impl SettingsManagerGetCompactionSettingsModel {
    pub fn new(provider: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            id: id.into(),
        }
    }
}

pub type SettingsManagerGetCompactionReserveTokensModel = SettingsManagerGetCompactionSettingsModel;
pub type SettingsManagerGetCompactionKeepRecentTokensModel =
    SettingsManagerGetCompactionSettingsModel;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsManagerGetCompactionSettingsResult {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsManagerGetBranchSummarySettingsResult {
    pub reserve_tokens: u64,
    pub skip_prompt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsManagerGetRetrySettingsResult {
    pub enabled: bool,
    pub max_retries: i64,
    pub base_delay_ms: i64,
    pub max_agent_delay_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsManagerGetProviderRetrySettingsResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<i64>,
    pub max_retry_delay_ms: i64,
}

open_string_enum! {
    /// `"fork" | "tree" | "none"`; `getDoubleEscapeAction()` returns the stored value unchecked.
    pub enum SettingsManagerGetDoubleEscapeActionResult {
        Fork => "fork",
        Tree => "tree",
        None => "none",
    }
}

// ============================================================================
// SettingsManager
// ============================================================================

#[derive(Default)]
struct ModifiedFields {
    fields: IndexSet<String>,
    nested: IndexMap<String, IndexSet<String>>,
}

impl ModifiedFields {
    fn mark(&mut self, field: &str, nested_key: Option<&str>) {
        self.fields.insert(field.to_string());
        if let Some(nested_key) = nested_key {
            self.nested
                .entry(field.to_string())
                .or_default()
                .insert(nested_key.to_string());
        }
    }

    fn clear(&mut self) {
        self.fields.clear();
        self.nested.clear();
    }

    fn snapshot(&self) -> (IndexSet<String>, IndexMap<String, IndexSet<String>>) {
        (self.fields.clone(), self.nested.clone())
    }
}

struct State {
    global_settings: JsonMap,
    project_settings: JsonMap,
    settings: JsonMap,
    project_trusted: bool,
    /// Global fields modified during this session.
    modified: ModifiedFields,
    /// Project fields modified during this session.
    modified_project: ModifiedFields,
    /// Global settings file had parse errors.
    global_settings_load_error: Option<Arc<anyhow::Error>>,
    /// Project settings file had parse errors.
    project_settings_load_error: Option<Arc<anyhow::Error>>,
    errors: Vec<SettingsError>,
}

impl State {
    fn remerge(&mut self) {
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
    }
}

struct LoadResult {
    settings: JsonMap,
    error: Option<Arc<anyhow::Error>>,
}

pub struct SettingsManager {
    storage: Arc<dyn SettingsStorage>,
    settings_paths: SettingsPaths,
    state: Mutex<State>,
    /// Serializes writes (the TS write queue).
    write_lock: Mutex<()>,
}

impl SettingsManager {
    #[allow(clippy::too_many_arguments)]
    fn construct(
        storage: Arc<dyn SettingsStorage>,
        initial_global: JsonMap,
        initial_project: JsonMap,
        global_load_error: Option<Arc<anyhow::Error>>,
        project_load_error: Option<Arc<anyhow::Error>>,
        initial_errors: Vec<SettingsError>,
        project_trusted: bool,
        settings_paths: SettingsPaths,
    ) -> Self {
        let mut state = State {
            global_settings: initial_global,
            project_settings: initial_project,
            settings: JsonMap::new(),
            project_trusted,
            modified: ModifiedFields::default(),
            modified_project: ModifiedFields::default(),
            global_settings_load_error: global_load_error,
            project_settings_load_error: project_load_error,
            errors: initial_errors,
        };
        state.remerge();
        Self {
            storage,
            settings_paths,
            state: Mutex::new(state),
            write_lock: Mutex::new(()),
        }
    }

    /// Create a SettingsManager that loads from files
    pub fn create(
        cwd: &str,
        agent_dir: Option<&str>,
        options: Option<&SettingsManagerCreateOptions>,
    ) -> Arc<SettingsManager> {
        let agent_dir = agent_dir.map(str::to_string).unwrap_or_else(get_agent_dir);
        let resolved_cwd = resolve_path(cwd, None, None);
        let resolved_agent_dir = resolve_path(&agent_dir, None, None);
        let storage = Arc::new(FileSettingsStorage::new(&resolved_cwd, &resolved_agent_dir));
        let mut paths = SettingsPaths::new();
        paths.insert(
            SettingsScope::Global,
            node_join(&[&resolved_agent_dir, "settings.json"]),
        );
        paths.insert(
            SettingsScope::Project,
            node_join(&[&resolved_cwd, CONFIG_DIR_NAME, "settings.json"]),
        );
        Self::from_storage_with_paths(storage, options, paths)
    }

    /// Create a SettingsManager from an arbitrary storage backend
    pub fn from_storage(
        storage: Arc<dyn SettingsStorage>,
        options: Option<&SettingsManagerCreateOptions>,
    ) -> Arc<SettingsManager> {
        Self::from_storage_with_paths(storage, options, SettingsPaths::new())
    }

    /// Create a manager while retaining optional file paths for reported storage errors.
    fn from_storage_with_paths(
        storage: Arc<dyn SettingsStorage>,
        options: Option<&SettingsManagerCreateOptions>,
        settings_paths: SettingsPaths,
    ) -> Arc<SettingsManager> {
        let project_trusted = options.and_then(|o| o.project_trusted).unwrap_or(true);
        let global_load = Self::try_load_from_storage(&*storage, SettingsScope::Global, true);
        let project_load =
            Self::try_load_from_storage(&*storage, SettingsScope::Project, project_trusted);
        let mut initial_errors = Vec::new();
        if let Some(error) = &global_load.error {
            initial_errors.push(to_settings_error(
                SettingsScope::Global,
                error.clone(),
                settings_paths.get(&SettingsScope::Global),
            ));
        }
        if let Some(error) = &project_load.error {
            initial_errors.push(to_settings_error(
                SettingsScope::Project,
                error.clone(),
                settings_paths.get(&SettingsScope::Project),
            ));
        }
        Arc::new(Self::construct(
            storage,
            global_load.settings,
            project_load.settings,
            global_load.error,
            project_load.error,
            initial_errors,
            project_trusted,
            settings_paths,
        ))
    }

    /// Create an in-memory SettingsManager (no file I/O)
    pub fn in_memory(
        settings: Option<&Settings>,
        options: Option<&SettingsManagerCreateOptions>,
    ) -> Arc<SettingsManager> {
        let map = settings.map(settings_to_json).unwrap_or_default();
        Self::in_memory_json(map, options)
    }

    /// [`SettingsManager::in_memory`] from raw settings JSON (keeps keys and values the typed
    /// [`Settings`] cannot represent).
    pub fn in_memory_json(
        settings: JsonMap,
        options: Option<&SettingsManagerCreateOptions>,
    ) -> Arc<SettingsManager> {
        let storage = Arc::new(InMemorySettingsStorage::new());
        let initial_settings = Self::migrate_settings(settings);
        let content = crate::utils::js_json::js_json_stringify_pretty(&Value::Object(initial_settings), "  ");
        let _ = storage.with_lock(SettingsScope::Global, &mut |_| Ok(Some(content.clone())));
        Self::from_storage(storage, options)
    }

    fn load_from_storage(
        storage: &dyn SettingsStorage,
        scope: SettingsScope,
        project_trusted: bool,
    ) -> anyhow::Result<JsonMap> {
        if scope == SettingsScope::Project && !project_trusted {
            return Ok(JsonMap::new());
        }

        let mut content: Option<String> = None;
        storage.with_lock(scope, &mut |current| {
            content = current;
            Ok(None)
        })?;

        let Some(content) = content.filter(|c| !c.is_empty()) else {
            return Ok(JsonMap::new());
        };
        Ok(Self::migrate_settings(Self::parse_settings_object(&content)?))
    }

    fn parse_settings_object(content: &str) -> anyhow::Result<JsonMap> {
        let parsed: Value = crate::utils::js_json::js_json_parse(&strip_bom(content))
            .map_err(|message| anyhow::Error::new(JsError::new("SyntaxError", message)))?;
        match parsed {
            Value::Object(map) => Ok(map),
            // Arrays are objects in JS: the migrations find nothing and the merge spreads the
            // entries by index.
            Value::Array(items) => {
                Ok(items.into_iter().enumerate().map(|(i, v)| (i.to_string(), v)).collect())
            }
            other => Err(anyhow::Error::new(JsError::new(
                "TypeError",
                format!("Cannot use 'in' operator to search for 'queueMode' in {}", js_string(&other)),
            ))),
        }
    }

    fn try_load_from_storage(
        storage: &dyn SettingsStorage,
        scope: SettingsScope,
        project_trusted: bool,
    ) -> LoadResult {
        match Self::load_from_storage(storage, scope, project_trusted) {
            Ok(settings) => LoadResult {
                settings,
                error: None,
            },
            Err(error) => LoadResult {
                settings: JsonMap::new(),
                error: Some(Arc::new(error)),
            },
        }
    }

    /// Migrate old settings format to new format
    pub fn migrate_settings(mut settings: JsonMap) -> JsonMap {
        // Migrate queueMode -> steeringMode
        if settings.contains_key("queueMode") && !settings.contains_key("steeringMode") {
            let queue_mode = settings.shift_remove("queueMode").unwrap_or(Value::Null);
            settings.insert("steeringMode".to_string(), queue_mode);
        }

        // Migrate legacy websockets boolean -> transport enum
        if !settings.contains_key("transport")
            && let Some(Value::Bool(websockets)) = settings.get("websockets").cloned()
        {
            settings.insert(
                "transport".to_string(),
                Value::String(if websockets { "websocket" } else { "sse" }.to_string()),
            );
            settings.shift_remove("websockets");
        }

        // Migrate old skills object format to new array format
        if let Some(Value::Object(skills_settings)) = settings.get("skills").cloned() {
            if let Some(enable) = skills_settings.get("enableSkillCommands")
                && !settings.contains_key("enableSkillCommands")
            {
                settings.insert("enableSkillCommands".to_string(), enable.clone());
            }
            match skills_settings.get("customDirectories") {
                Some(Value::Array(dirs)) if !dirs.is_empty() => {
                    settings.insert("skills".to_string(), Value::Array(dirs.clone()));
                }
                _ => {
                    settings.shift_remove("skills");
                }
            }
        }

        // Migrate retry.maxDelayMs -> retry.provider.maxRetryDelayMs
        if let Some(Value::Object(retry_settings)) = settings.get_mut("retry") {
            let provider_settings: Option<JsonMap> = match retry_settings.get("provider") {
                Some(Value::Object(p)) => Some(p.clone()),
                Some(Value::Array(items)) => Some(
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, v)| (i.to_string(), v.clone()))
                        .collect(),
                ),
                _ => None,
            };
            if let Some(max_delay) = retry_settings.get("maxDelayMs").filter(|v| v.is_number())
                && provider_settings
                    .as_ref()
                    .and_then(|p| p.get("maxRetryDelayMs"))
                    .is_none_or(Value::is_null)
            {
                let mut provider = provider_settings.unwrap_or_default();
                provider.insert("maxRetryDelayMs".to_string(), max_delay.clone());
                retry_settings.insert("provider".to_string(), Value::Object(provider));
            }
            retry_settings.shift_remove("maxDelayMs");
        }

        settings
    }

    /// A copy of the effective settings: global and project settings merged, with overrides.
    pub fn get_settings(&self) -> Settings {
        settings_from_json(&self.state.lock().settings)
    }

    pub fn get_global_settings(&self) -> Settings {
        settings_from_json(&self.state.lock().global_settings)
    }

    pub fn get_project_settings(&self) -> Settings {
        settings_from_json(&self.state.lock().project_settings)
    }

    /// The effective settings as raw JSON (includes keys the typed [`Settings`] drops).
    pub fn get_settings_json(&self) -> JsonMap {
        self.state.lock().settings.clone()
    }

    /// The global settings as raw JSON.
    pub fn get_global_settings_json(&self) -> JsonMap {
        self.state.lock().global_settings.clone()
    }

    /// The project settings as raw JSON.
    pub fn get_project_settings_json(&self) -> JsonMap {
        self.state.lock().project_settings.clone()
    }

    pub fn is_project_trusted(&self) -> bool {
        self.state.lock().project_trusted
    }

    pub fn set_project_trusted(&self, trusted: bool) {
        {
            let mut state = self.state.lock();
            if state.project_trusted == trusted {
                return;
            }
            state.project_trusted = trusted;
            state.modified_project.clear();

            if !trusted {
                state.project_settings = JsonMap::new();
                state.project_settings_load_error = None;
                state.remerge();
                return;
            }
        }

        let project_load =
            Self::try_load_from_storage(&*self.storage, SettingsScope::Project, trusted);
        let mut state = self.state.lock();
        state.project_settings = project_load.settings;
        state.project_settings_load_error = project_load.error.clone();
        if let Some(error) = project_load.error {
            self.record_error(&mut state, SettingsScope::Project, error);
        }
        state.remerge();
    }

    pub async fn reload(&self) -> anyhow::Result<()> {
        self.reload_sync();
        Ok(())
    }

    /// Synchronous [`SettingsManager::reload`].
    pub fn reload_sync(&self) {
        let _write = self.write_lock.lock();
        let global_load = Self::try_load_from_storage(&*self.storage, SettingsScope::Global, true);
        let project_trusted = self.state.lock().project_trusted;
        let project_load =
            Self::try_load_from_storage(&*self.storage, SettingsScope::Project, project_trusted);

        let mut state = self.state.lock();
        match global_load.error {
            None => {
                state.global_settings = global_load.settings;
                state.global_settings_load_error = None;
            }
            Some(error) => {
                state.global_settings_load_error = Some(error.clone());
                self.record_error(&mut state, SettingsScope::Global, error);
            }
        }

        state.modified.clear();
        state.modified_project.clear();

        match project_load.error {
            None => {
                state.project_settings = project_load.settings;
                state.project_settings_load_error = None;
            }
            Some(error) => {
                state.project_settings_load_error = Some(error.clone());
                self.record_error(&mut state, SettingsScope::Project, error);
            }
        }

        state.remerge();
    }

    /// Apply additional overrides on top of current settings
    pub fn apply_overrides(&self, overrides: &Settings) {
        self.apply_overrides_json(&settings_to_json(overrides));
    }

    /// [`SettingsManager::apply_overrides`] with raw settings JSON.
    pub fn apply_overrides_json(&self, overrides: &JsonMap) {
        let mut state = self.state.lock();
        state.settings = deep_merge_settings(&state.settings, overrides);
    }

    fn record_error(&self, state: &mut State, scope: SettingsScope, error: Arc<anyhow::Error>) {
        state.errors.push(to_settings_error(
            scope,
            error,
            self.settings_paths.get(&scope),
        ));
    }

    /// Run a write for `scope` (the TS write queue task): check trust for project writes, persist,
    /// and clear the modified fields of the scope on success; failures are recorded as errors.
    fn enqueue_write(
        &self,
        scope: SettingsScope,
        snapshot: JsonMap,
        modified_fields: IndexSet<String>,
        modified_nested: IndexMap<String, IndexSet<String>>,
    ) {
        // Callers hold `write_lock` from before they take the snapshot until the write is done, so
        // snapshots are persisted in the order they were taken.
        let result = (|| -> anyhow::Result<()> {
            if scope == SettingsScope::Project && !self.state.lock().project_trusted {
                anyhow::bail!("Project is not trusted; refusing to write project settings");
            }
            self.persist_scoped_settings(scope, &snapshot, &modified_fields, &modified_nested)
        })();
        let mut state = self.state.lock();
        match result {
            Ok(()) => match scope {
                SettingsScope::Global => state.modified.clear(),
                SettingsScope::Project => state.modified_project.clear(),
            },
            Err(error) => self.record_error(&mut state, scope, Arc::new(error)),
        }
    }

    fn persist_scoped_settings(
        &self,
        scope: SettingsScope,
        snapshot_settings: &JsonMap,
        modified_fields: &IndexSet<String>,
        modified_nested_fields: &IndexMap<String, IndexSet<String>>,
    ) -> anyhow::Result<()> {
        self.storage.with_lock(scope, &mut |current| {
            let current_file_settings = match current.filter(|c| !c.is_empty()) {
                Some(content) => Self::migrate_settings(Self::parse_settings_object(&content)?),
                None => JsonMap::new(),
            };
            let mut merged_settings = current_file_settings.clone();
            for field in modified_fields {
                let value = snapshot_settings.get(field);
                match (modified_nested_fields.get(field), value) {
                    (Some(nested_modified), Some(Value::Object(in_memory_nested))) => {
                        let mut merged_nested = current_file_settings
                            .get(field)
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        for nested_key in nested_modified {
                            match in_memory_nested.get(nested_key) {
                                Some(v) => {
                                    merged_nested.insert(nested_key.clone(), v.clone());
                                }
                                None => {
                                    merged_nested.shift_remove(nested_key);
                                }
                            }
                        }
                        merged_settings.insert(field.clone(), Value::Object(merged_nested));
                    }
                    (_, Some(value)) => {
                        merged_settings.insert(field.clone(), value.clone());
                    }
                    (_, None) => {
                        // `undefined` is omitted by JSON.stringify.
                        merged_settings.shift_remove(field);
                    }
                }
            }
            Ok(Some(crate::utils::js_json::js_json_stringify_pretty(
                &Value::Object(merged_settings),
                "  ",
            )))
        })
    }

    /// Mutate global settings, mark the field modified and save.
    fn update_global(&self, field: &str, nested_key: Option<&str>, update: impl FnOnce(&mut JsonMap)) {
        let _write = self.write_lock.lock();
        let snapshot = {
            let mut state = self.state.lock();
            update(&mut state.global_settings);
            state.modified.mark(field, nested_key);
            state.remerge();
            if state.global_settings_load_error.is_some() {
                return;
            }
            let (fields, nested) = state.modified.snapshot();
            (state.global_settings.clone(), fields, nested)
        };
        self.enqueue_write(SettingsScope::Global, snapshot.0, snapshot.1, snapshot.2);
    }

    fn set_global_value(&self, field: &str, value: Option<Value>) {
        self.update_global(field, None, |global| match value {
            Some(value) => {
                global.insert(field.to_string(), value);
            }
            None => {
                // Assigning `undefined` keeps the key in place; JSON.stringify drops it.
                global.shift_remove(field);
            }
        });
    }

    /// `if (!global[field]) global[field] = {}; global[field][key] = value`.
    fn set_global_nested(&self, field: &str, key: &str, value: Value) {
        self.update_global(field, Some(key), |global| {
            let obj = ensure_object(global, field, false);
            obj.insert(key.to_string(), value);
        });
    }

    fn assert_project_trusted_for_write(&self) -> anyhow::Result<()> {
        if !self.state.lock().project_trusted {
            anyhow::bail!("Project is not trusted; refusing to write project settings");
        }
        Ok(())
    }

    fn update_project_settings(
        &self,
        field: &str,
        update: impl FnOnce(&mut JsonMap),
    ) -> anyhow::Result<()> {
        self.assert_project_trusted_for_write()?;
        let _write = self.write_lock.lock();
        let snapshot = {
            let mut state = self.state.lock();
            let mut project_settings = state.project_settings.clone();
            update(&mut project_settings);
            state.modified_project.mark(field, None);
            // saveProjectSettings
            state.project_settings = project_settings;
            state.remerge();
            if state.project_settings_load_error.is_some() {
                return Ok(());
            }
            let (fields, nested) = state.modified_project.snapshot();
            (state.project_settings.clone(), fields, nested)
        };
        self.enqueue_write(SettingsScope::Project, snapshot.0, snapshot.1, snapshot.2);
        Ok(())
    }

    /// Wait for queued writes. Writes are synchronous in the Rust port, so this only waits for a
    /// write running on another thread.
    pub async fn flush(&self) -> anyhow::Result<()> {
        drop(self.write_lock.lock());
        Ok(())
    }

    pub fn drain_errors(&self) -> Vec<SettingsError> {
        std::mem::take(&mut self.state.lock().errors)
    }

    fn with_settings<R>(&self, f: impl FnOnce(&JsonMap) -> R) -> R {
        f(&self.state.lock().settings)
    }

    pub fn get_last_changelog_version(&self) -> Option<String> {
        self.get_string("lastChangelogVersion")
    }

    fn get_string(&self, key: &str) -> Option<String> {
        self.with_settings(|s| present(s, key).and_then(Value::as_str).map(str::to_string))
    }

    pub fn set_last_changelog_version(&self, version: &str) {
        self.set_global_value("lastChangelogVersion", Some(Value::String(version.to_string())));
    }

    pub fn get_session_dir(&self) -> Option<String> {
        self.get_string("sessionDir").map(|dir| {
            if dir.is_empty() {
                dir
            } else {
                normalize_path(&dir, None)
            }
        })
    }

    pub fn get_default_provider(&self) -> Option<String> {
        self.get_string("defaultProvider")
    }

    pub fn get_default_model(&self) -> Option<String> {
        self.get_string("defaultModel")
    }

    pub fn set_default_provider(&self, provider: &str) {
        self.set_global_value("defaultProvider", Some(Value::String(provider.to_string())));
    }

    pub fn set_default_model(&self, model_id: &str) {
        self.set_global_value("defaultModel", Some(Value::String(model_id.to_string())));
    }

    pub fn set_default_model_and_provider(&self, provider: &str, model_id: &str) {
        let _write = self.write_lock.lock();
        let snapshot = {
            let mut state = self.state.lock();
            state.global_settings.insert(
                "defaultProvider".to_string(),
                Value::String(provider.to_string()),
            );
            state
                .global_settings
                .insert("defaultModel".to_string(), Value::String(model_id.to_string()));
            state.modified.mark("defaultProvider", None);
            state.modified.mark("defaultModel", None);
            state.remerge();
            if state.global_settings_load_error.is_some() {
                return;
            }
            let (fields, nested) = state.modified.snapshot();
            (state.global_settings.clone(), fields, nested)
        };
        self.enqueue_write(SettingsScope::Global, snapshot.0, snapshot.1, snapshot.2);
    }

    /// `this.settings.<key> || "one-at-a-time"`: any truthy value is returned as is.
    fn get_queue_mode(&self, key: &str) -> SettingsManagerGetSteeringModeResult {
        self.with_settings(|s| match s.get(key) {
            value @ Some(raw) if is_truthy(value) => SettingsManagerGetSteeringModeResult::from_value(raw.clone()),
            _ => SettingsManagerGetSteeringModeResult::OneAtATime,
        })
    }

    pub fn get_steering_mode(&self) -> SettingsManagerGetSteeringModeResult {
        self.get_queue_mode("steeringMode")
    }

    pub fn set_steering_mode(&self, mode: SettingsManagerGetSteeringModeResult) {
        self.set_global_value("steeringMode", Some(mode.to_value()));
    }

    pub fn get_follow_up_mode(&self) -> SettingsManagerGetSteeringModeResult {
        self.get_queue_mode("followUpMode")
    }

    pub fn set_follow_up_mode(&self, mode: SettingsManagerGetSteeringModeResult) {
        self.set_global_value("followUpMode", Some(mode.to_value()));
    }

    pub fn get_theme_setting(&self) -> Option<String> {
        self.get_string("theme")
    }

    pub fn get_theme(&self) -> Option<String> {
        self.get_theme_setting().filter(|t| !t.contains('/'))
    }

    pub fn set_theme(&self, theme: &str) {
        self.set_global_value("theme", Some(Value::String(theme.to_string())));
    }

    /// The stored `defaultThinkingLevel`, unrecognized values included (TS returns it unchecked).
    pub fn get_default_thinking_level_setting(&self) -> Option<ThinkingLevelSetting> {
        self.with_settings(|s| present(s, "defaultThinkingLevel").cloned().map(ThinkingLevelSetting::from_value))
    }

    /// [`Self::get_default_thinking_level_setting`] as the level a session uses
    /// (see [`ThinkingLevelSetting::effective_level`]).
    pub fn get_default_thinking_level(&self) -> Option<ThinkingLevel> {
        self.get_default_thinking_level_setting().map(|l| l.effective_level())
    }

    pub fn set_default_thinking_level(&self, level: ThinkingLevel) {
        self.set_global_value("defaultThinkingLevel", Some(to_json(&level)));
    }

    pub fn get_model_thinking_level(
        &self,
        provider: &str,
        model_id: &str,
    ) -> Option<ThinkingLevel> {
        self.get_model_thinking_level_setting(provider, model_id)
            .map(|l| l.effective_level())
    }

    /// The stored per-model thinking level, unrecognized values included.
    pub fn get_model_thinking_level_setting(&self, provider: &str, model_id: &str) -> Option<ThinkingLevelSetting> {
        let key = format!("{provider}/{model_id}");
        self.with_settings(|s| {
            nested_raw(s, "modelThinkingLevels", &key)
                .cloned()
                .map(ThinkingLevelSetting::from_value)
        })
    }

    /// Per-model thinking levels as sessions use them (see
    /// [`ThinkingLevelSetting::effective_level`]); null entries are skipped (`?? fallback`).
    pub fn get_all_model_thinking_levels(&self) -> IndexMap<String, ThinkingLevel> {
        self.get_all_model_thinking_level_settings()
            .into_iter()
            .filter(|(_, v)| *v != ThinkingLevelSetting::Other(Value::Null))
            .map(|(k, v)| (k, v.effective_level()))
            .collect()
    }

    /// `{ ...this.settings.modelThinkingLevels }`: the stored values, unrecognized ones included.
    pub fn get_all_model_thinking_level_settings(&self) -> IndexMap<String, ThinkingLevelSetting> {
        self.with_settings(|s| {
            present(s, "modelThinkingLevels")
                .and_then(Value::as_object)
                .map(|levels| {
                    levels
                        .iter()
                        .map(|(k, v)| (k.clone(), ThinkingLevelSetting::from_value(v.clone())))
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    pub fn set_model_thinking_level(&self, provider: &str, model_id: &str, level: ThinkingLevel) {
        let key = format!("{provider}/{model_id}");
        self.update_global("modelThinkingLevels", None, |global| {
            let levels = ensure_object(global, "modelThinkingLevels", false);
            levels.insert(key, to_json(&level));
        });
    }

    pub fn remove_model_thinking_level(&self, provider: &str, model_id: &str) {
        let key = format!("{provider}/{model_id}");
        {
            let state = self.state.lock();
            if !is_truthy(state.global_settings.get("modelThinkingLevels")) {
                return;
            }
        }
        self.update_global("modelThinkingLevels", None, |global| {
            let now_empty = match global.get_mut("modelThinkingLevels") {
                Some(Value::Object(levels)) => {
                    levels.shift_remove(&key);
                    levels.is_empty()
                }
                _ => false,
            };
            if now_empty {
                global.shift_remove("modelThinkingLevels");
            }
        });
    }

    /// `this.settings.transport ?? "auto"`: other values are returned as is.
    pub fn get_transport(&self) -> TransportSetting {
        self.with_settings(|s| {
            present(s, "transport")
                .cloned()
                .map(TransportSetting::from_value)
                .unwrap_or(TransportSetting::Auto)
        })
    }

    pub fn set_transport(&self, transport: &TransportSetting) {
        self.set_global_value("transport", Some(to_json(transport)));
    }

    pub fn get_session_share_destination(&self) -> SessionShareDestination {
        match self.get_string("sessionShareDestination").as_deref() {
            Some("gist") => SessionShareDestination::Gist,
            Some("radius") => SessionShareDestination::Radius,
            _ => SessionShareDestination::Auto,
        }
    }

    pub fn set_session_share_destination(&self, destination: &SessionShareDestination) {
        self.set_global_value("sessionShareDestination", Some(to_json(destination)));
    }

    pub fn get_compaction_enabled(&self) -> bool {
        self.with_settings(|s| bool_or(nested(s, "compaction", "enabled"), defaults::compaction::ENABLED))
    }

    pub fn set_compaction_enabled(&self, enabled: bool) {
        self.set_global_nested("compaction", "enabled", Value::Bool(enabled));
    }

    fn get_compaction_token_setting(
        &self,
        field: &str,
        model: Option<&SettingsManagerGetCompactionSettingsModel>,
    ) -> anyhow::Result<u64> {
        fn is_valid(value: &Value) -> bool {
            match value.as_f64() {
                Some(f) => f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER && f >= 0.0,
                None => false,
            }
        }
        // Non-finite numbers cannot be represented in JSON; they arrive as null.
        self.with_settings(|s| {
            let compaction = s.get("compaction").and_then(Value::as_object);
            let ordinary = compaction.and_then(|c| c.get(field));
            if let Some(ordinary) = ordinary
                && !is_valid(ordinary)
            {
                anyhow::bail!(
                    "Invalid compaction.{field} setting: {}. Expected a non-negative safe integer.",
                    js_string(ordinary)
                );
            }

            let model_key = model.map(|m| format!("{}/{}", m.provider, m.id));
            let entry = match &model_key {
                Some(key) => compaction
                    .and_then(|c| c.get("modelOverrides"))
                    .and_then(Value::as_object)
                    .and_then(|o| o.get(key)),
                None => None,
            };
            if let Some(entry) = entry
                && !is_mergeable_object(entry)
            {
                anyhow::bail!(
                    "Invalid compaction.modelOverrides[\"{}\"] setting: {}. Expected an object.",
                    model_key.as_deref().unwrap_or(""),
                    js_string(entry)
                );
            }
            let override_value = entry.and_then(Value::as_object).and_then(|o| o.get(field));
            if let Some(override_value) = override_value
                && !is_valid(override_value)
            {
                anyhow::bail!(
                    "Invalid compaction.modelOverrides[\"{}\"].{field} setting: {}. Expected a non-negative safe integer.",
                    model_key.as_deref().unwrap_or(""),
                    js_string(override_value)
                );
            }
            let default = if field == "reserveTokens" {
                defaults::compaction::RESERVE_TOKENS
            } else {
                defaults::compaction::KEEP_RECENT_TOKENS
            };
            Ok(override_value
                .or(ordinary)
                .and_then(Value::as_f64)
                .map(|f| f as u64)
                .unwrap_or(default))
        })
    }

    pub fn get_compaction_reserve_tokens(
        &self,
        model: Option<&SettingsManagerGetCompactionReserveTokensModel>,
    ) -> anyhow::Result<u64> {
        self.get_compaction_token_setting("reserveTokens", model)
    }

    pub fn get_compaction_keep_recent_tokens(
        &self,
        model: Option<&SettingsManagerGetCompactionKeepRecentTokensModel>,
    ) -> anyhow::Result<u64> {
        self.get_compaction_token_setting("keepRecentTokens", model)
    }

    /// Resolve each token setting through model override, ordinary setting, then built-in default.
    pub fn get_compaction_settings(
        &self,
        model: Option<&SettingsManagerGetCompactionSettingsModel>,
    ) -> anyhow::Result<SettingsManagerGetCompactionSettingsResult> {
        Ok(SettingsManagerGetCompactionSettingsResult {
            enabled: self.get_compaction_enabled(),
            reserve_tokens: self.get_compaction_reserve_tokens(model)?,
            keep_recent_tokens: self.get_compaction_keep_recent_tokens(model)?,
        })
    }

    pub fn get_branch_summary_settings(&self) -> SettingsManagerGetBranchSummarySettingsResult {
        self.with_settings(|s| SettingsManagerGetBranchSummarySettingsResult {
            reserve_tokens: number_u64(
                nested(s, "branchSummary", "reserveTokens"),
                defaults::branch_summary::RESERVE_TOKENS,
            ),
            skip_prompt: bool_or(
                nested(s, "branchSummary", "skipPrompt"),
                defaults::branch_summary::SKIP_PROMPT,
            ),
        })
    }

    pub fn get_branch_summary_skip_prompt(&self) -> bool {
        self.get_branch_summary_settings().skip_prompt
    }

    pub fn get_retry_enabled(&self) -> bool {
        self.with_settings(|s| bool_or(nested(s, "retry", "enabled"), defaults::retry::ENABLED))
    }

    pub fn set_retry_enabled(&self, enabled: bool) {
        self.set_global_nested("retry", "enabled", Value::Bool(enabled));
    }

    pub fn get_retry_settings(&self) -> SettingsManagerGetRetrySettingsResult {
        self.with_settings(|s| SettingsManagerGetRetrySettingsResult {
            enabled: bool_or(nested(s, "retry", "enabled"), defaults::retry::ENABLED),
            max_retries: number_i64(
                nested(s, "retry", "maxRetries"),
                defaults::retry::MAX_RETRIES as i64,
            ),
            base_delay_ms: number_i64(
                nested(s, "retry", "baseDelayMs"),
                defaults::retry::BASE_DELAY_MS as i64,
            ),
            max_agent_delay_ms: number_i64(
                nested(s, "retry", "maxAgentDelayMs"),
                defaults::retry::MAX_AGENT_DELAY_MS as i64,
            ),
        })
    }

    pub fn get_http_idle_timeout_ms(&self) -> anyhow::Result<i64> {
        self.with_settings(|s| {
            Ok(
                parse_timeout_setting(s.get("httpIdleTimeoutMs"), "httpIdleTimeoutMs")?
                    .unwrap_or(crate::core::http_dispatcher::DEFAULT_HTTP_IDLE_TIMEOUT_MS),
            )
        })
    }

    pub fn set_http_idle_timeout_ms(&self, timeout_ms: i64) -> anyhow::Result<()> {
        if timeout_ms < 0 {
            anyhow::bail!("Invalid httpIdleTimeoutMs setting: {timeout_ms}");
        }
        self.set_global_value("httpIdleTimeoutMs", Some(Value::from(timeout_ms)));
        Ok(())
    }

    /// Read from global settings only because warming costs money.
    pub fn get_cache_warming_mode(&self) -> CacheWarmingMode {
        let state = self.state.lock();
        match state.global_settings.get("cacheWarming").and_then(Value::as_str) {
            Some("off") => CacheWarmingMode::Off,
            Some("idle") => CacheWarmingMode::Idle,
            Some("streaming") => CacheWarmingMode::Streaming,
            _ => CacheWarmingMode::Streaming,
        }
    }

    pub fn set_cache_warming_mode(&self, mode: &CacheWarmingMode) {
        self.set_global_value("cacheWarming", Some(to_json(mode)));
    }

    pub fn get_provider_retry_settings(&self) -> SettingsManagerGetProviderRetrySettingsResult {
        self.with_settings(|s| {
            let provider = nested(s, "retry", "provider").and_then(Value::as_object);
            let get = |key: &str| {
                provider
                    .and_then(|p| p.get(key))
                    .and_then(Value::as_f64)
                    .map(|f| f as i64)
            };
            SettingsManagerGetProviderRetrySettingsResult {
                timeout_ms: get("timeoutMs"),
                max_retries: get("maxRetries"),
                max_retry_delay_ms: get("maxRetryDelayMs")
                    .unwrap_or(defaults::retry::provider::MAX_RETRY_DELAY_MS as i64),
            }
        })
    }

    pub fn get_web_socket_connect_timeout_ms(&self) -> anyhow::Result<Option<i64>> {
        self.with_settings(|s| {
            parse_timeout_setting(s.get("websocketConnectTimeoutMs"), "websocketConnectTimeoutMs")
        })
    }

    pub fn get_hide_thinking_block(&self) -> bool {
        self.with_settings(|s| bool_or(present(s, "hideThinkingBlock"), defaults::HIDE_THINKING_BLOCK))
    }

    pub fn get_show_cache_miss_notices(&self) -> bool {
        self.with_settings(|s| {
            bool_or(present(s, "showCacheMissNotices"), defaults::SHOW_CACHE_MISS_NOTICES)
        })
    }

    pub fn get_external_editor_command(&self) -> String {
        if let Some(configured) = self.get_string("externalEditor")
            && !configured.trim().is_empty()
        {
            return configured;
        }
        let env_editor = std::env::var("VISUAL")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty()));
        if let Some(editor) = env_editor {
            return editor;
        }
        if cfg!(windows) { "notepad" } else { "nano" }.to_string()
    }

    pub fn set_hide_thinking_block(&self, hide: bool) {
        self.set_global_value("hideThinkingBlock", Some(Value::Bool(hide)));
    }

    pub fn set_show_cache_miss_notices(&self, show: bool) {
        self.set_global_value("showCacheMissNotices", Some(Value::Bool(show)));
    }

    pub fn get_shell_path(&self) -> Option<String> {
        self.get_string("shellPath").map(|path| {
            if path.is_empty() {
                path
            } else {
                normalize_path(&path, None)
            }
        })
    }

    pub fn set_shell_path(&self, path: Option<&str>) {
        self.set_global_value("shellPath", path.map(|p| Value::String(p.to_string())));
    }

    pub fn get_quiet_startup(&self) -> QuietStartup {
        self.with_settings(|s| match s.get("quietStartup") {
            Some(Value::Bool(true)) => QuietStartup::Bool(true),
            Some(Value::String(v)) if v == "header" => QuietStartup::Header("header".to_string()),
            _ => QuietStartup::Bool(defaults::QUIET_STARTUP),
        })
    }

    pub fn set_quiet_startup(&self, quiet: &QuietStartup) {
        self.set_global_value("quietStartup", Some(to_json(quiet)));
    }

    pub fn get_default_project_trust(&self) -> DefaultProjectTrust {
        let state = self.state.lock();
        match state
            .global_settings
            .get("defaultProjectTrust")
            .and_then(Value::as_str)
        {
            Some("always") => DefaultProjectTrust::Always,
            Some("never") => DefaultProjectTrust::Never,
            _ => DefaultProjectTrust::Ask,
        }
    }

    pub fn set_default_project_trust(&self, default_project_trust: &DefaultProjectTrust) {
        self.set_global_value("defaultProjectTrust", Some(to_json(default_project_trust)));
    }

    pub fn get_shell_command_prefix(&self) -> Option<String> {
        self.get_string("shellCommandPrefix")
    }

    pub fn set_shell_command_prefix(&self, prefix: Option<&str>) {
        self.set_global_value(
            "shellCommandPrefix",
            prefix.map(|p| Value::String(p.to_string())),
        );
    }

    pub fn get_npm_command(&self) -> Option<Vec<String>> {
        self.with_settings(|s| {
            let value = s.get("npmCommand");
            if is_truthy(value) {
                Some(string_array(value))
            } else {
                None
            }
        })
    }

    pub fn set_npm_command(&self, command: Option<&[String]>) {
        self.set_global_value("npmCommand", command.map(|c| to_json(&c.to_vec())));
    }

    pub fn get_collapse_changelog(&self) -> bool {
        self.with_settings(|s| bool_or(present(s, "collapseChangelog"), defaults::COLLAPSE_CHANGELOG))
    }

    pub fn set_collapse_changelog(&self, collapse: bool) {
        self.set_global_value("collapseChangelog", Some(Value::Bool(collapse)));
    }

    pub fn get_enable_install_telemetry(&self) -> bool {
        self.with_settings(|s| {
            bool_or(present(s, "enableInstallTelemetry"), defaults::ENABLE_INSTALL_TELEMETRY)
        })
    }

    pub fn set_enable_install_telemetry(&self, enabled: bool) {
        self.set_global_value("enableInstallTelemetry", Some(Value::Bool(enabled)));
    }

    pub fn get_enable_analytics(&self) -> bool {
        self.with_settings(|s| bool_or(present(s, "enableAnalytics"), defaults::ENABLE_ANALYTICS))
    }

    pub fn get_tracking_id(&self) -> Option<String> {
        self.get_string("trackingId")
    }

    /// Set the analytics opt-in preference; generates a tracking identifier on first opt-in
    pub fn set_enable_analytics(&self, enabled: bool) {
        let _write = self.write_lock.lock();
        let snapshot = {
            let mut state = self.state.lock();
            state
                .global_settings
                .insert("enableAnalytics".to_string(), Value::Bool(enabled));
            state.modified.mark("enableAnalytics", None);
            if enabled && !is_truthy(state.global_settings.get("trackingId")) {
                state.global_settings.insert(
                    "trackingId".to_string(),
                    Value::String(uuid::Uuid::new_v4().to_string()),
                );
                state.modified.mark("trackingId", None);
            }
            state.remerge();
            if state.global_settings_load_error.is_some() {
                return;
            }
            let (fields, nested) = state.modified.snapshot();
            (state.global_settings.clone(), fields, nested)
        };
        self.enqueue_write(SettingsScope::Global, snapshot.0, snapshot.1, snapshot.2);
    }

    /// Stable ID of this installation, e.g. sent to OpenAI as its agent host ID.
    /// Created on first use. Project settings are ignored so a committed project
    /// settings file cannot give every clone the same ID.
    pub fn get_or_create_device_id(&self) -> String {
        {
            let state = self.state.lock();
            if let Some(Value::String(id)) = state.global_settings.get("deviceId")
                && !id.is_empty()
            {
                return id.clone();
            }
        }
        let device_id = uuid::Uuid::new_v4().to_string();
        self.set_global_value("deviceId", Some(Value::String(device_id.clone())));
        device_id
    }

    pub fn get_packages(&self) -> Vec<PackageSource> {
        self.with_settings(|s| {
            present(s, "packages")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| serde_json::from_value(v.clone()).ok())
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    pub fn set_packages(&self, packages: &[PackageSource]) {
        self.set_global_value("packages", Some(to_json(&packages.to_vec())));
    }

    pub fn set_project_packages(&self, packages: &[PackageSource]) -> anyhow::Result<()> {
        let value = to_json(&packages.to_vec());
        self.update_project_settings("packages", |settings| {
            settings.insert("packages".to_string(), value);
        })
    }

    fn get_string_list(&self, key: &str) -> Vec<String> {
        self.with_settings(|s| string_array(present(s, key)))
    }

    fn set_project_string_list(&self, key: &str, paths: &[String]) -> anyhow::Result<()> {
        let value = to_json(&paths.to_vec());
        self.update_project_settings(key, |settings| {
            settings.insert(key.to_string(), value);
        })
    }

    pub fn get_extension_paths(&self) -> Vec<String> {
        self.get_string_list("extensions")
    }

    pub fn set_extension_paths(&self, paths: &[String]) {
        self.set_global_value("extensions", Some(to_json(&paths.to_vec())));
    }

    pub fn set_project_extension_paths(&self, paths: &[String]) -> anyhow::Result<()> {
        self.set_project_string_list("extensions", paths)
    }

    pub fn get_skill_paths(&self) -> Vec<String> {
        self.get_string_list("skills")
    }

    pub fn set_skill_paths(&self, paths: &[String]) {
        self.set_global_value("skills", Some(to_json(&paths.to_vec())));
    }

    pub fn set_project_skill_paths(&self, paths: &[String]) -> anyhow::Result<()> {
        self.set_project_string_list("skills", paths)
    }

    pub fn get_prompt_template_paths(&self) -> Vec<String> {
        self.get_string_list("prompts")
    }

    pub fn set_prompt_template_paths(&self, paths: &[String]) {
        self.set_global_value("prompts", Some(to_json(&paths.to_vec())));
    }

    pub fn set_project_prompt_template_paths(&self, paths: &[String]) -> anyhow::Result<()> {
        self.set_project_string_list("prompts", paths)
    }

    pub fn get_theme_paths(&self) -> Vec<String> {
        self.get_string_list("themes")
    }

    pub fn set_theme_paths(&self, paths: &[String]) {
        self.set_global_value("themes", Some(to_json(&paths.to_vec())));
    }

    pub fn set_project_theme_paths(&self, paths: &[String]) -> anyhow::Result<()> {
        self.set_project_string_list("themes", paths)
    }

    pub fn get_enable_skill_commands(&self) -> bool {
        self.with_settings(|s| {
            bool_or(present(s, "enableSkillCommands"), defaults::ENABLE_SKILL_COMMANDS)
        })
    }

    pub fn set_enable_skill_commands(&self, enabled: bool) {
        self.set_global_value("enableSkillCommands", Some(Value::Bool(enabled)));
    }

    pub fn get_thinking_budgets(&self) -> Option<ThinkingBudgetsSettings> {
        self.with_settings(|s| {
            present(s, "thinkingBudgets").and_then(|v| serde_json::from_value(v.clone()).ok())
        })
    }

    pub fn get_terminal_capability_overrides(&self) -> CapabilityOverrides {
        self.with_settings(|s| {
            let mut overrides = CapabilityOverrides::default();
            match nested_raw(s, "terminal", "images") {
                Some(Value::String(v)) if v == "kitty" => {
                    overrides.images = Some(Some(ImageProtocol::Kitty))
                }
                Some(Value::String(v)) if v == "iterm2" => {
                    overrides.images = Some(Some(ImageProtocol::Iterm2))
                }
                Some(Value::Bool(false)) => overrides.images = Some(None),
                _ => {}
            }
            if let Some(Value::Bool(b)) = nested_raw(s, "terminal", "trueColor") {
                overrides.true_color = Some(*b);
            }
            if let Some(Value::Bool(b)) = nested_raw(s, "terminal", "hyperlinks") {
                overrides.hyperlinks = Some(*b);
            }
            overrides
        })
    }

    pub fn get_show_images(&self) -> bool {
        self.with_settings(|s| bool_or(nested(s, "terminal", "showImages"), defaults::terminal::SHOW_IMAGES))
    }

    pub fn set_show_images(&self, show: bool) {
        self.set_global_nested("terminal", "showImages", Value::Bool(show));
    }

    pub fn get_image_width_cells(&self) -> i64 {
        self.with_settings(|s| match finite_number(nested_raw(s, "terminal", "imageWidthCells")) {
            Some(width) => (width.floor() as i64).max(1),
            None => defaults::terminal::IMAGE_WIDTH_CELLS as i64,
        })
    }

    pub fn set_image_width_cells(&self, width: i64) {
        self.set_global_nested("terminal", "imageWidthCells", Value::from(width.max(1)));
    }

    pub fn get_clear_on_shrink(&self) -> bool {
        // Settings takes precedence, then env var, then default false
        if let Some(value) = self.with_settings(|s| nested_raw(s, "terminal", "clearOnShrink").cloned()) {
            return is_truthy(Some(&value));
        }
        if std::env::var("PI_CLEAR_ON_SHRINK").as_deref() == Ok("1") {
            return true;
        }
        defaults::terminal::CLEAR_ON_SHRINK
    }

    pub fn set_clear_on_shrink(&self, enabled: bool) {
        self.set_global_nested("terminal", "clearOnShrink", Value::Bool(enabled));
    }

    pub fn get_show_terminal_progress(&self) -> bool {
        self.with_settings(|s| {
            bool_or(
                nested(s, "terminal", "showTerminalProgress"),
                defaults::terminal::SHOW_TERMINAL_PROGRESS,
            )
        })
    }

    pub fn set_show_terminal_progress(&self, enabled: bool) {
        self.set_global_nested("terminal", "showTerminalProgress", Value::Bool(enabled));
    }

    pub fn get_tui_mode(&self) -> TuiMode {
        match self.get_string("tuiMode").as_deref() {
            Some("regular") => TuiMode::Regular,
            _ => TuiMode::Fullscreen,
        }
    }

    pub fn set_tui_mode(&self, mode: &TuiMode) {
        self.set_global_value("tuiMode", Some(to_json(mode)));
    }

    pub fn get_fullscreen_exit_output(&self) -> FullscreenExitOutput {
        match self.get_string("fullscreenExitOutput").as_deref() {
            Some("resume-hint") => FullscreenExitOutput::ResumeHint,
            _ => FullscreenExitOutput::Transcript,
        }
    }

    pub fn set_fullscreen_exit_output(&self, output: &FullscreenExitOutput) {
        self.set_global_value("fullscreenExitOutput", Some(to_json(output)));
    }

    pub fn get_fullscreen_scrollbar(&self) -> ScrollViewScrollbar {
        match self.get_string("fullscreenScrollbar").as_deref() {
            Some("always") => ScrollViewScrollbar::Always,
            Some("hidden") => ScrollViewScrollbar::Hidden,
            _ => ScrollViewScrollbar::Auto,
        }
    }

    pub fn set_fullscreen_scrollbar(&self, mode: ScrollViewScrollbar) {
        let value = match mode {
            ScrollViewScrollbar::Always => "always",
            ScrollViewScrollbar::Hidden => "hidden",
            ScrollViewScrollbar::Auto => "auto",
        };
        self.set_global_value("fullscreenScrollbar", Some(Value::String(value.to_string())));
    }

    pub fn get_fullscreen_copy_on_select(&self) -> bool {
        self.with_settings(|s| {
            bool_or(present(s, "fullscreenCopyOnSelect"), defaults::FULLSCREEN_COPY_ON_SELECT)
        })
    }

    pub fn set_fullscreen_copy_on_select(&self, enabled: bool) {
        self.set_global_value("fullscreenCopyOnSelect", Some(Value::Bool(enabled)));
    }

    pub fn get_fullscreen_wheel_scroll_lines(&self) -> WheelScrollLines {
        self.with_settings(|s| match finite_number(s.get("fullscreenWheelScrollLines")) {
            Some(lines) => WheelScrollLines::Lines(lines.floor().clamp(1.0, 100.0) as u32),
            None => WheelScrollLines::Auto("auto".to_string()),
        })
    }

    pub fn set_fullscreen_wheel_scroll_lines(&self, lines: &WheelScrollLines) {
        let value = match lines {
            WheelScrollLines::Lines(n) => Value::from((*n).clamp(1, 100)),
            WheelScrollLines::Auto(_) => Value::String("auto".to_string()),
        };
        self.set_global_value("fullscreenWheelScrollLines", Some(value));
    }

    pub fn get_image_auto_resize(&self) -> bool {
        self.with_settings(|s| bool_or(nested(s, "images", "autoResize"), defaults::images::AUTO_RESIZE))
    }

    pub fn set_image_auto_resize(&self, enabled: bool) {
        self.set_global_nested("images", "autoResize", Value::Bool(enabled));
    }

    pub fn get_block_images(&self) -> bool {
        self.with_settings(|s| bool_or(nested(s, "images", "blockImages"), defaults::images::BLOCK_IMAGES))
    }

    pub fn set_block_images(&self, blocked: bool) {
        self.set_global_nested("images", "blockImages", Value::Bool(blocked));
    }

    pub fn get_enabled_models(&self) -> Option<Vec<String>> {
        self.with_settings(|s| present(s, "enabledModels").map(|v| string_array(Some(v))))
    }

    /// The resolved `defaultTools` selection, or undefined when no settings layer sets it.
    pub fn get_default_tools(&self) -> Option<Vec<String>> {
        self.with_settings(|s| {
            let tools = s.get("defaultTools")?;
            Some(resolve_default_tools(&string_array(Some(tools))))
        })
    }

    pub fn set_enabled_models(&self, patterns: Option<&[String]>) {
        self.set_global_value("enabledModels", patterns.map(|p| to_json(&p.to_vec())));
    }

    /// `this.settings.doubleEscapeAction ?? "tree"`: other values are returned as is.
    pub fn get_double_escape_action(&self) -> SettingsManagerGetDoubleEscapeActionResult {
        self.with_settings(|s| {
            present(s, "doubleEscapeAction")
                .cloned()
                .map(SettingsManagerGetDoubleEscapeActionResult::from_value)
                .unwrap_or(SettingsManagerGetDoubleEscapeActionResult::Tree)
        })
    }

    pub fn set_double_escape_action(&self, action: SettingsManagerGetDoubleEscapeActionResult) {
        self.set_global_value("doubleEscapeAction", Some(action.to_value()));
    }

    pub fn get_tree_filter_mode(&self) -> FilterMode {
        self.with_settings(|s| {
            present(s, "treeFilterMode")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or(FilterMode::Default)
        })
    }

    pub fn set_tree_filter_mode(&self, mode: FilterMode) {
        self.set_global_value("treeFilterMode", Some(to_json(&mode)));
    }

    pub fn get_show_hardware_cursor(&self) -> bool {
        match self.with_settings(|s| present(s, "showHardwareCursor").cloned()) {
            Some(value) => is_truthy(Some(&value)),
            None => std::env::var("PI_HARDWARE_CURSOR").as_deref() == Ok("1"),
        }
    }

    pub fn set_show_hardware_cursor(&self, enabled: bool) {
        self.set_global_value("showHardwareCursor", Some(Value::Bool(enabled)));
    }

    pub fn get_editor_padding_x(&self) -> i64 {
        self.with_settings(|s| {
            number_i64(present(s, "editorPaddingX"), defaults::EDITOR_PADDING_X as i64)
        })
    }

    pub fn set_editor_padding_x(&self, padding: i64) {
        self.set_global_value("editorPaddingX", Some(Value::from(padding.clamp(0, 3))));
    }

    pub fn get_output_pad(&self) -> i64 {
        self.with_settings(|s| {
            if s.get("outputPad").and_then(Value::as_f64) == Some(0.0) {
                0
            } else {
                defaults::OUTPUT_PAD as i64
            }
        })
    }

    pub fn set_output_pad(&self, padding: i64) {
        self.set_global_value("outputPad", Some(Value::from(padding)));
    }

    pub fn get_autocomplete_max_visible(&self) -> i64 {
        self.with_settings(|s| {
            number_i64(
                present(s, "autocompleteMaxVisible"),
                defaults::AUTOCOMPLETE_MAX_VISIBLE as i64,
            )
        })
    }

    pub fn set_autocomplete_max_visible(&self, max_visible: i64) {
        self.set_global_value(
            "autocompleteMaxVisible",
            Some(Value::from(max_visible.clamp(3, 20))),
        );
    }

    pub fn get_code_block_indent(&self) -> String {
        self.with_settings(|s| {
            nested(s, "markdown", "codeBlockIndent")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| defaults::markdown::CODE_BLOCK_INDENT.to_string())
        })
    }

    pub fn get_mermaid_rendering_mode(&self) -> MermaidRenderingMode {
        self.with_settings(|s| match nested(s, "markdown", "mermaid").and_then(Value::as_str) {
            Some("off") => MermaidRenderingMode::Off,
            Some("final") => MermaidRenderingMode::Final,
            _ => MermaidRenderingMode::Streaming,
        })
    }

    pub fn set_mermaid_rendering_mode(&self, mode: &MermaidRenderingMode) {
        let value = to_json(mode);
        self.update_global("markdown", Some("mermaid"), |global| {
            // `markdown ??= {}`
            let obj = ensure_object(global, "markdown", true);
            obj.insert("mermaid".to_string(), value);
        });
    }

    pub fn get_warnings(&self) -> WarningSettings {
        self.with_settings(|s| {
            present(s, "warnings")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default()
        })
    }

    pub fn set_warnings(&self, warnings: &WarningSettings) {
        self.set_global_value("warnings", Some(to_json(warnings)));
    }
}

/// Ensure `map[key]` is an object: `if (!map[key]) map[key] = {}` (or `??=` with `nullish_only`).
/// A value of another type is replaced, since property assignment on it would throw in TS.
fn ensure_object<'a>(map: &'a mut JsonMap, key: &str, nullish_only: bool) -> &'a mut JsonMap {
    let _ = nullish_only;
    let replace = !matches!(map.get(key), Some(Value::Object(_)));
    if replace {
        map.insert(key.to_string(), Value::Object(JsonMap::new()));
    }
    match map.get_mut(key) {
        Some(Value::Object(obj)) => obj,
        _ => unreachable!("ensured object"),
    }
}

#[cfg(test)]
#[path = "settings_manager_tests.rs"]
mod tests;
