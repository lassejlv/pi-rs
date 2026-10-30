//! Port of `packages/coding-agent/src/core/settings-schema.ts`.

use crate::modes::interactive::components::tree_selector::FilterMode;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;

/// The settings JSON Schema (TypeBox `SettingsSchema`).
///
/// Built from the generated `schemas/settings.schema.json` artifact of the TS package (embedded
/// next to this module) with the shared `$defs` inlined again, which yields the TypeBox schema.
pub static SETTINGS_SCHEMA: LazyLock<Value> =
    LazyLock::new(|| strip_generated_schema(SETTINGS_SCHEMA_ARTIFACT.get()));

/// The published `settings.schema.json` artifact (with `$schema`, `$id` and `$defs`).
pub const SETTINGS_SCHEMA_ARTIFACT: pi_embed::Embedded = pi_embed::include_str!("schemas/settings.schema.json");

/// Parse a generated schema artifact, drop the artifact headers (`$schema`, `$id`, `$comment`) and
/// inline its `$defs` references, keeping per-use annotations.
pub(crate) fn strip_generated_schema(artifact: &str) -> Value {
    let mut root: Value = serde_json::from_str(artifact).expect("embedded schema artifact is valid JSON");
    let defs = root
        .as_object_mut()
        .and_then(|obj| obj.remove("$defs"))
        .unwrap_or(Value::Null);
    if let Some(obj) = root.as_object_mut() {
        obj.shift_remove("$schema");
        obj.shift_remove("$id");
        obj.shift_remove("$comment");
    }
    inline_schema_refs(&root, &defs)
}

fn inline_schema_refs(value: &Value, defs: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(|v| inline_schema_refs(v, defs)).collect()),
        Value::Object(obj) => {
            if let Some(Value::String(reference)) = obj.get("$ref")
                && let Some(name) = reference.strip_prefix("#/$defs/")
                && let Some(def) = defs.get(name)
            {
                let mut inlined = match inline_schema_refs(def, defs) {
                    Value::Object(map) => map,
                    other => return other,
                };
                for (key, child) in obj {
                    if key != "$ref" {
                        inlined.insert(key.clone(), child.clone());
                    }
                }
                return Value::Object(inlined);
            }
            Value::Object(
                obj.iter()
                    .map(|(k, v)| (k.clone(), inline_schema_refs(v, defs)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SettingsInputSteeringMode {
    #[serde(rename = "all")]
    All,
    #[serde(rename = "one-at-a-time")]
    OneAtATime,
}

impl SettingsInputSteeringMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::OneAtATime => "one-at-a-time",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsInputRetry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<i64>,
    /// Exponential backoff base delay in milliseconds: 2s, 4s, 8s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_delay_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_delay_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderRetrySettings>,
    /// Legacy retry delay setting. Use provider.maxRetryDelayMs instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delay_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingsInputQuietStartup {
    Bool(bool),
    /// Always "header".
    Header(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsInputSkillsVariant2 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_skill_commands: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_directories: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingsInputSkills {
    List(Vec<String>),
    SettingsInputSkillsVariant2(SettingsInputSkillsVariant2),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SettingsInputDoubleEscapeAction {
    #[serde(rename = "fork")]
    Fork,
    #[serde(rename = "tree")]
    Tree,
    #[serde(rename = "none")]
    None,
}

impl SettingsInputDoubleEscapeAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fork => "fork",
            Self::Tree => "tree",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsInputThinkingBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal: Option<i64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub low: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub medium: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub high: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingsInputOutputPad {
    Number(i64),
    Number2(i64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingsInputHttpIdleTimeoutMs {
    Number(i64),
    /// Always "disabled".
    Disabled(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingsInputWebsocketConnectTimeoutMs {
    Number(i64),
    /// Always "disabled".
    Disabled(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SettingsInputFullscreenScrollbar {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "always")]
    Always,
    #[serde(rename = "hidden")]
    Hidden,
}

impl SettingsInputFullscreenScrollbar {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Hidden => "hidden",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingsInputFullscreenWheelScrollLines {
    Number(i64),
    /// Always "auto".
    Auto(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsInput {
    /// JSON Schema reference.
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_changelog_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_thinking_level: Option<Value>,
    /// Per-model default thinking level overrides keyed by "provider/modelId".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_thinking_levels: Option<IndexMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportSetting>,
    /// Destination used by /share. "auto" uses Radius when authenticated and otherwise creates a GitHub gist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_share_destination: Option<SessionShareDestination>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering_mode: Option<SettingsInputSteeringMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up_mode: Option<SettingsInputSteeringMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_summary: Option<BranchSummarySettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<SettingsInputRetry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hide_thinking_block: Option<bool>,
    /// Show cache cost and provider recovery notices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_cache_miss_notices: Option<bool>,
    /// Command for Ctrl+G external editor; takes precedence over VISUAL and EDITOR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_editor: Option<String>,
    /// Custom shell path, for example for Cygwin on Windows, with support for leading ~ expansion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    /// When true, hide all startup output. When "header", keep only the startup header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_startup: Option<SettingsInputQuietStartup>,
    /// Global setting only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_project_trust: Option<DefaultProjectTrust>,
    /// Prefix prepended to every bash command, for example to enable shell aliases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_command_prefix: Option<String>,
    /// Command used for npm package lookup and installation, in argv form such as ["mise", "exec", "node@20", "--", "npm"].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm_command: Option<Vec<String>>,
    /// Show the condensed changelog after update; use /changelog for the full changelog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collapse_changelog: Option<bool>,
    /// Send an anonymous version and update ping after changelog-detected updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_install_telemetry: Option<bool>,
    /// Opt in to analytics data sharing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_analytics: Option<bool>,
    /// Analytics tracking identifier, generated when analytics is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking_id: Option<String>,
    /// npm or git package sources, as strings or objects with resource filtering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages: Option<Vec<PackageSource>>,
    /// Local extension file paths or directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    /// Stable installation UUID, generated when authentication first needs it. Global only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// Local skill file paths or directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<SettingsInputSkills>,
    /// Local prompt template file paths or directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    /// Local theme file paths or directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
    /// Register skills as /skill:name commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_skill_commands: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<ImageSettings>,
    /// Model patterns for cycling, in the same format as the --models CLI flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_models: Option<Vec<String>>,
    /// Initial tool selection. Plain names replace the inherited selection; +name and -name entries add or remove tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools: Option<Vec<String>>,
    /// Action for double-escape with an empty editor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub double_escape_action: Option<SettingsInputDoubleEscapeAction>,
    /// Default filter when opening /tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_filter_mode: Option<FilterMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<SettingsInputThinkingBudgets>,
    /// Horizontal padding for the input editor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_padding_x: Option<i64>,
    /// Horizontal padding for transcript content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_pad: Option<SettingsInputOutputPad>,
    /// Maximum visible items in the autocomplete dropdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autocomplete_max_visible: Option<i64>,
    /// Show the terminal cursor while still positioning it for IME.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_hardware_cursor: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<MarkdownSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warnings: Option<WarningSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codemode: Option<CodemodeSettings>,
    /// Custom session storage directory, in the same format as the --session-dir CLI flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
    /// Proxy URL applied as HTTP_PROXY and HTTPS_PROXY for Pi-managed HTTP clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_proxy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_idle_timeout_ms: Option<SettingsInputHttpIdleTimeoutMs>,
    /// Cache-warming profile. "idle" also warms between agent runs. Global only because each refresh costs money.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_warming: Option<CacheWarmingMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket_connect_timeout_ms: Option<SettingsInputWebsocketConnectTimeoutMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_mode: Option<TuiMode>,
    /// No effect in regular TUI mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_exit_output: Option<FullscreenExitOutput>,
    /// No effect in regular TUI mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_scrollbar: Option<SettingsInputFullscreenScrollbar>,
    /// No effect in regular TUI mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_copy_on_select: Option<bool>,
    /// Lines scrolled per wheel event in fullscreen mode; numeric values are clamped from 1 to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_wheel_scroll_lines: Option<SettingsInputFullscreenWheelScrollLines>,
    /// Legacy setting migrated to steeringMode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_mode: Option<SettingsInputSteeringMode>,
    /// Legacy setting migrated to transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websockets: Option<bool>,
}

// TS: extends Static<typeof CompactionModelOverrideSchema> (inherited fields inlined)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionModelOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
}

// TS: extends Static<typeof CompactionSettingsSchema> (inherited fields inlined)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
    /// Per-model overrides keyed by exact "provider/modelId" strings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<IndexMap<String, CompactionModelOverride>>,
}

// TS: extends Static<typeof BranchSummarySettingsSchema> (inherited fields inlined)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummarySettings {
    /// Tokens reserved for the prompt and LLM response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    /// When true, skips the "Summarize branch?" prompt and defaults to no summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_prompt: Option<bool>,
}

// TS: extends Static<typeof ProviderRetrySettingsSchema> (inherited fields inlined)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRetrySettings {
    /// SDK or provider request timeout in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<i64>,
    /// SDK or provider retry attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<i64>,
    /// Maximum server-requested delay before failing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<i64>,
}

// TS: extends Omit<Static<typeof RetrySettingsSchema>, "maxDelayMs"> (inherited fields inlined)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<i64>,
    /// Exponential backoff base delay in milliseconds: 2s, 4s, 8s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_delay_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_delay_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderRetrySettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TerminalSettingsHyperlinks {
    Bool(bool),
    /// Always "auto".
    Auto(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TerminalSettingsImages {
    /// Always "kitty".
    Kitty(String),
    /// Always "iterm2".
    Iterm2(String),
    /// Always "auto".
    Auto(String),
    Bool(bool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TerminalSettingsTrueColor {
    Bool(bool),
    /// Always "auto".
    Auto(String),
}

// TS: extends Static<typeof TerminalSettingsSchema> (inherited fields inlined)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettings {
    /// Show images when the terminal supports them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_images: Option<bool>,
    /// Preferred inline image width in terminal cells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_width_cells: Option<i64>,
    /// Clear empty rows when content shrinks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_on_shrink: Option<bool>,
    /// Show OSC 9;4 terminal progress indicators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_terminal_progress: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hyperlinks: Option<TerminalSettingsHyperlinks>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<TerminalSettingsImages>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub true_color: Option<TerminalSettingsTrueColor>,
}

// TS: extends Static<typeof ImageSettingsSchema> (inherited fields inlined)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSettings {
    /// Resize images to 2000x2000 maximum for better model compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_resize: Option<bool>,
    /// When true, prevents all images from being sent to LLM providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_images: Option<bool>,
}

// TS: extends Static<typeof ThinkingBudgetsSettingsSchema> (inherited fields inlined)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingBudgetsSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal: Option<i64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub low: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub medium: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub high: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MermaidRenderingModeValue {
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "final")]
    Final,
    #[serde(rename = "streaming")]
    Streaming,
}

impl MermaidRenderingModeValue {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Final => "final",
            Self::Streaming => "streaming",
        }
    }
}

pub type MermaidRenderingMode = MermaidRenderingModeValue;

// TS: extends Static<typeof MarkdownSettingsSchema> (inherited fields inlined)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_block_indent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mermaid: Option<MermaidRenderingMode>,
}

// TS: extends Static<typeof WarningSettingsSchema> (inherited fields inlined)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarningSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_extra_usage: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CodemodeModeValue {
    #[serde(rename = "on")]
    On,
    #[serde(rename = "only")]
    Only,
}

impl CodemodeModeValue {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Only => "only",
        }
    }
}

pub type CodemodeMode = CodemodeModeValue;

// TS: extends Static<typeof CodemodeSettingsSchema> (inherited fields inlined)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CodemodeMode>,
    /// Estimated tokens available for inline codemode tool declarations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_budget: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DefaultProjectTrustValue {
    #[serde(rename = "ask")]
    Ask,
    #[serde(rename = "always")]
    Always,
    #[serde(rename = "never")]
    Never,
}

impl DefaultProjectTrustValue {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

pub type DefaultProjectTrust = DefaultProjectTrustValue;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QuietStartupValue {
    Bool(bool),
    /// Always "header".
    Header(String),
}

pub type QuietStartup = QuietStartupValue;

/// Declares a string-literal union whose TS getter hands back unrecognized values verbatim
/// (`return this.settings.x ?? default`): the known literals are variants and anything else is kept
/// as raw JSON in `Other`, so it serializes back unchanged.
macro_rules! open_string_enum {
    ($(#[$meta:meta])* $vis:vis enum $name:ident { $($(#[$vmeta:meta])* $variant:ident => $literal:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        $vis enum $name {
            $($(#[$vmeta])* $variant,)+
            /// A value outside the union, kept as the raw JSON value.
            Other(serde_json::Value),
        }

        impl $name {
            /// Map a JSON value, keeping unrecognized values verbatim.
            pub fn from_value(value: serde_json::Value) -> Self {
                match value.as_str() {
                    $(Some($literal) => Self::$variant,)+
                    _ => Self::Other(value),
                }
            }

            /// The literal for a known variant.
            pub fn known_str(&self) -> Option<&'static str> {
                match self {
                    $(Self::$variant => Some($literal),)+
                    Self::Other(_) => None,
                }
            }

            /// The JSON value TS holds for this setting.
            pub fn to_value(&self) -> serde_json::Value {
                match self {
                    Self::Other(value) => value.clone(),
                    known => serde_json::Value::String(known.known_str().unwrap_or_default().to_string()),
                }
            }

            /// `String(value)` in JS (the literal, or the raw value's JS string form).
            pub fn to_js_string(&self) -> String {
                pi_agent::types::js_string(&self.to_value())
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.to_value().serialize(serializer)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Ok(Self::from_value(serde_json::Value::deserialize(deserializer)?))
            }
        }
    };
}
pub(crate) use open_string_enum;

open_string_enum! {
    /// `TransportSetting`; `getTransport()` returns whatever the settings hold.
    pub enum TransportSettingValue {
        Auto => "auto",
        Sse => "sse",
        Websocket => "websocket",
        WebsocketCached => "websocket-cached",
    }
}

pub type TransportSetting = TransportSettingValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionShareDestinationValue {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "gist")]
    Gist,
    #[serde(rename = "radius")]
    Radius,
}

impl SessionShareDestinationValue {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Gist => "gist",
            Self::Radius => "radius",
        }
    }
}

pub type SessionShareDestination = SessionShareDestinationValue;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageSourceVariant2 {
    pub source: String,
    /// When false, start empty and only apply explicit resource patterns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoload: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PackageSourceValue {
    String(String),
    PackageSourceVariant2(PackageSourceVariant2),
}

pub type PackageSource = PackageSourceValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FullscreenExitOutputValue {
    #[serde(rename = "transcript")]
    Transcript,
    #[serde(rename = "resume-hint")]
    ResumeHint,
}

impl FullscreenExitOutputValue {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Transcript => "transcript",
            Self::ResumeHint => "resume-hint",
        }
    }
}

pub type FullscreenExitOutput = FullscreenExitOutputValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TuiModeValue {
    #[serde(rename = "regular")]
    Regular,
    #[serde(rename = "fullscreen")]
    Fullscreen,
}

impl TuiModeValue {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Fullscreen => "fullscreen",
        }
    }
}

pub type TuiMode = TuiModeValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CacheWarmingModeValue {
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "streaming")]
    Streaming,
    #[serde(rename = "idle")]
    Idle,
}

impl CacheWarmingModeValue {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Streaming => "streaming",
            Self::Idle => "idle",
        }
    }
}

pub type CacheWarmingMode = CacheWarmingModeValue;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingsQuietStartup {
    Bool(bool),
    /// Always "header".
    Header(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsThinkingBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal: Option<i64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub low: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub medium: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub high: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingsOutputPad {
    Number(i64),
    Number2(i64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingsHttpIdleTimeoutMs {
    Number(i64),
    /// Always "disabled".
    Disabled(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingsWebsocketConnectTimeoutMs {
    Number(i64),
    /// Always "disabled".
    Disabled(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingsFullscreenWheelScrollLines {
    Number(i64),
    /// Always "auto".
    Auto(String),
}

// TS: extends Omit<SettingsInput, "queueMode" | "retry" | "skills" | "websockets"> (inherited fields inlined)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// JSON Schema reference.
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_changelog_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_thinking_level: Option<Value>,
    /// Per-model default thinking level overrides keyed by "provider/modelId".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_thinking_levels: Option<IndexMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportSetting>,
    /// Destination used by /share. "auto" uses Radius when authenticated and otherwise creates a GitHub gist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_share_destination: Option<SessionShareDestination>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering_mode: Option<SettingsInputSteeringMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up_mode: Option<SettingsInputSteeringMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_summary: Option<BranchSummarySettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hide_thinking_block: Option<bool>,
    /// Show cache cost and provider recovery notices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_cache_miss_notices: Option<bool>,
    /// Command for Ctrl+G external editor; takes precedence over VISUAL and EDITOR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_editor: Option<String>,
    /// Custom shell path, for example for Cygwin on Windows, with support for leading ~ expansion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    /// When true, hide all startup output. When "header", keep only the startup header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_startup: Option<SettingsQuietStartup>,
    /// Global setting only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_project_trust: Option<DefaultProjectTrust>,
    /// Prefix prepended to every bash command, for example to enable shell aliases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_command_prefix: Option<String>,
    /// Command used for npm package lookup and installation, in argv form such as ["mise", "exec", "node@20", "--", "npm"].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm_command: Option<Vec<String>>,
    /// Show the condensed changelog after update; use /changelog for the full changelog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collapse_changelog: Option<bool>,
    /// Send an anonymous version and update ping after changelog-detected updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_install_telemetry: Option<bool>,
    /// Opt in to analytics data sharing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_analytics: Option<bool>,
    /// Analytics tracking identifier, generated when analytics is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking_id: Option<String>,
    /// npm or git package sources, as strings or objects with resource filtering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages: Option<Vec<PackageSource>>,
    /// Local extension file paths or directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    /// Stable installation UUID, generated when authentication first needs it. Global only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// Local prompt template file paths or directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    /// Local theme file paths or directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
    /// Register skills as /skill:name commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_skill_commands: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<ImageSettings>,
    /// Model patterns for cycling, in the same format as the --models CLI flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_models: Option<Vec<String>>,
    /// Initial tool selection. Plain names replace the inherited selection; +name and -name entries add or remove tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools: Option<Vec<String>>,
    /// Action for double-escape with an empty editor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub double_escape_action: Option<SettingsInputDoubleEscapeAction>,
    /// Default filter when opening /tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_filter_mode: Option<FilterMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<SettingsThinkingBudgets>,
    /// Horizontal padding for the input editor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_padding_x: Option<i64>,
    /// Horizontal padding for transcript content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_pad: Option<SettingsOutputPad>,
    /// Maximum visible items in the autocomplete dropdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autocomplete_max_visible: Option<i64>,
    /// Show the terminal cursor while still positioning it for IME.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_hardware_cursor: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<MarkdownSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warnings: Option<WarningSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codemode: Option<CodemodeSettings>,
    /// Custom session storage directory, in the same format as the --session-dir CLI flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
    /// Proxy URL applied as HTTP_PROXY and HTTPS_PROXY for Pi-managed HTTP clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_proxy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_idle_timeout_ms: Option<SettingsHttpIdleTimeoutMs>,
    /// Cache-warming profile. "idle" also warms between agent runs. Global only because each refresh costs money.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_warming: Option<CacheWarmingMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket_connect_timeout_ms: Option<SettingsWebsocketConnectTimeoutMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_mode: Option<TuiMode>,
    /// No effect in regular TUI mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_exit_output: Option<FullscreenExitOutput>,
    /// No effect in regular TUI mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_scrollbar: Option<SettingsInputFullscreenScrollbar>,
    /// No effect in regular TUI mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_copy_on_select: Option<bool>,
    /// Lines scrolled per wheel event in fullscreen mode; numeric values are clamped from 1 to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_wheel_scroll_lines: Option<SettingsFullscreenWheelScrollLines>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetrySettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// Keys not covered by the schema (`additionalProperties: true`).
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}
