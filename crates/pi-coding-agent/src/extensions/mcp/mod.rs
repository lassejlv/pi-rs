//! Port of `packages/coding-agent/src/extensions/mcp/index.ts`.
//!
//! Built-in MCP integration.
//!
//! Connects the servers from `mcp.json` and the servers extensions register with
//! `pi.registerMcpServer()` when a session starts, and servers registered later right away. A server
//! in `mcp.json` takes precedence over a registered server of the same name. Connections run in the
//! background: the first prompt waits only for servers with `direct` tools, and codemode scripts,
//! `tool_search`, and the resource tools wait for the servers they need when they run. Tools are
//! registered as `mcp__<server>__<tool>`. By default (`"exposure": "codemode"`) the tools are only
//! callable from codemode scripts, which keeps MCP tools out of the model's tool declarations and
//! the codemode description: scripts find the tools with `searchTools()` and the server instructions
//! with `describeNamespace()`. The codemode tool is activated for that unless `autoEnableCodemode` is
//! false. `"deferred"` declares the tools to the model once the `tool_search` tool loads them, and
//! activates `tool_search` instead of codemode.
//! `"exposure": "direct"` declares them to the model right away, and `"hidden"` makes them
//! unreachable. `toolExposure` overrides the exposure of single tools. Servers with resources are
//! reached through Codex's `list_mcp_resources`, `list_mcp_resource_templates`, and
//! `read_mcp_resource` tools (resources.rs).
//!
//! Every call runs through pi's tool pipeline, so `tool_call`/`tool_result` hooks and permission
//! extensions apply to MCP tools the same way they do to built-in tools.
//!
//! Problems found at startup (config errors, failed connections, servers that need a sign-in) are
//! reported once. `/mcp` opens a manager to sign in, reconnect, enable or disable servers, and change
//! their exposure; the last two are saved to the `mcp.json` that defines the server, or apply to the
//! current session for registered servers.

pub mod cli;
pub mod config;
pub mod log;
pub mod oauth;
pub mod resources;
pub mod runtime;
pub mod tools;
pub mod ui;

#[cfg(test)]
mod tests;

use crate::compat::AutocompleteItem;
use crate::config::get_agent_dir;
use crate::core::extensions::types::{
    BeforeAgentStartEvent, BeforeAgentStartEventResult, CommandOptions, ExtensionAPIRef,
    ExtensionCommandContext, ExtensionContext, ExtensionContextHost, ExtensionFactory,
    ExtensionMode, ExtensionUIDialogOptions, McpServersChangeEvent, NotifyType, ReachesTools,
    SessionShutdownEvent, SessionStartEvent, ToolCallEvent, ToolDefinition, ToolExposure,
    ToolNamespace, ToolRenderers, TurnStartEvent, on_event,
};
use crate::core::mcp_servers::{McpServerConfig, mcp_namespace};
use crate::extensions::codemode::tool::CODEMODE_TOOL_NAME;
use crate::extensions::mcp::config::{
    LoadMcpConfigOptions, LoadedMcpConfig, McpExposure, McpServerConfigPatch, McpServerEntry,
    McpServerEntryScope, UpdateMcpServerConfigOptions, get_mcp_tool_exposure, load_mcp_config,
    update_mcp_server_config,
};
use crate::extensions::mcp::oauth::{McpOAuthCredentialStore, McpSignInPrompt};
use crate::extensions::mcp::resources::{
    CreateMcpResourceToolDefinitionsOptions, LIST_MCP_RESOURCE_TEMPLATES_TOOL,
    LIST_MCP_RESOURCES_TOOL, McpResourceServer, READ_MCP_RESOURCE_TOOL,
    create_mcp_resource_tool_definitions,
};
use crate::extensions::mcp::runtime::{
    McpServerConnection, McpServerConnectionOptions, McpServerLog, McpSignInCancelledError,
    ServerState, default_transport_factory, sign_in_mcp_server,
};
use crate::extensions::mcp::tools::{
    CreateMcpToolDefinitionOptions, McpToolCaller, create_mcp_tool_definition,
    create_mcp_tool_name, create_mcp_tool_name_with, create_mcp_tool_renderers,
};
use crate::extensions::mcp::ui::{McpMenu, McpMenuSubscribe, McpUi, show_mcp_manager};
use crate::extensions::tool_search::tool::TOOL_SEARCH_TOOL_NAME;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use pi_tui::components::select_list::SelectItem;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub use crate::extensions::mcp::runtime::McpTransportFactory;

/// Loads the MCP config for a session.
pub type McpLoadConfigFn = Arc<dyn Fn(ExtensionContext) -> LoadedMcpConfig + Send + Sync>;
/// Saves `/mcp` changes; fails when the file cannot be updated.
pub type McpUpdateConfigFn =
    Arc<dyn Fn(McpServerEntry, McpServerConfigPatch) -> anyhow::Result<()> + Send + Sync>;

#[derive(Clone, Default)]
pub struct McpExtensionOptions {
    /// Defaults to reading `mcp.json` from the agent directory and the trusted project.
    pub load_config: Option<McpLoadConfigFn>,
    /// Defaults to stdio and streamable HTTP transports built from the server config.
    pub create_transport: Option<McpTransportFactory>,
    /// Defaults to `mcp-auth.json` in the agent directory.
    pub credentials: Option<Arc<McpOAuthCredentialStore>>,
    /// File server log messages are appended to. Defaults to `mcp.log` in the agent directory.
    pub log_path: Option<String>,
    /// Opens the OAuth authorization URL. Defaults to the platform browser.
    pub open_url: Option<Arc<dyn Fn(String) + Send + Sync>>,
    /// Saves `/mcp` changes to the server's config file: its project `override` when set, else its
    /// `source`. Defaults to editing that `mcp.json`.
    pub update_config: Option<McpUpdateConfigFn>,
    /// How long the first prompt waits for servers with `direct` tools that are still connecting at
    /// startup, in milliseconds. Their tools become available when they connect. Other servers are
    /// waited for when a script or search needs them. Default: 10000.
    pub startup_wait_ms: Option<i64>,
}

const DEFAULT_STARTUP_WAIT_MS: i64 = 10_000;

const RESOURCE_TOOL_NAMES: [&str; 3] = [
    LIST_MCP_RESOURCES_TOOL,
    LIST_MCP_RESOURCE_TEMPLATES_TOOL,
    READ_MCP_RESOURCE_TOOL,
];

/// A settled-or-pending readiness signal; it never fails.
pub type McpReady = Shared<BoxFuture<'static, ()>>;

/// A configured server. Disabled servers have no connection.
#[derive(Clone)]
pub struct McpServer {
    pub entry: McpServerEntry,
    pub connection: Option<Arc<McpServerConnection>>,
    /// For servers extensions registered: the config as registered, to detect re-registrations.
    pub registered_config: Option<String>,
    /// Result of the last `/mcp` action that failed, shown in the manager.
    pub message: Option<String>,
    /// Identifies the current connection attempt; disabling or replacing it invalidates earlier work.
    pub attempt: Option<u64>,
    /// Settles when the connection started for the server connected or failed.
    pub ready: Option<McpReady>,
    /// Cleanup of a detached connection; a replacement waits for it before opening a transport.
    pub closing: Option<McpReady>,
}

impl McpServer {
    fn new(entry: McpServerEntry) -> Self {
        Self {
            entry,
            connection: None,
            registered_config: None,
            message: None,
            attempt: None,
            ready: None,
            closing: None,
        }
    }
}

type ServerRef = Arc<Mutex<McpServer>>;

/// Name of the system prompt section that lists the servers whose tools are not declared.
pub const MCP_SERVERS_SECTION: &str = "mcp_servers";

/// Characters of a server description in the section, as Codex allows for deferred namespaces.
const MAX_SERVER_DESCRIPTION_CHARS: i64 = 250;

/// Characters of the whole section. Descriptions shrink to fit; when the server lines alone do not
/// fit, the last servers are left out and counted in a closing line.
pub const MAX_SERVERS_SECTION_CHARS: i64 = 4096;

const EXPOSURE_DESCRIPTIONS: [(McpExposure, &str); 3] = [
    (
        McpExposure::Codemode,
        "called from codemode scripts, which find them with searchTools()",
    ),
    (
        McpExposure::Deferred,
        "not declared until tool_search loads them, then called directly; no codemode needed",
    ),
    (
        McpExposure::Direct,
        "declared to the model like built-in tools",
    ),
];

fn exposure_description(exposure: McpExposure) -> &'static str {
    EXPOSURE_DESCRIPTIONS
        .iter()
        .find(|(candidate, _)| *candidate == exposure)
        .map(|(_, description)| *description)
        .unwrap_or("unreachable")
}

/// `text.length` in JavaScript: UTF-16 code units.
fn js_len(text: &str) -> i64 {
    text.encode_utf16().count() as i64
}

/// `a.localeCompare(b)` (ICU root collation).
pub(crate) fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    pi_tui::collation::locale_compare(a, b)
}

fn first_line(text: &str) -> String {
    text.split('\n').next().unwrap_or("").to_string()
}

fn config_enabled(config: &McpServerConfig) -> Option<bool> {
    match config {
        McpServerConfig::McpStdioServerConfig(config) => config.enabled,
        McpServerConfig::McpHttpServerConfig(config) => config.enabled,
    }
}

fn config_description(config: &McpServerConfig) -> Option<String> {
    match config {
        McpServerConfig::McpStdioServerConfig(config) => config.description.clone(),
        McpServerConfig::McpHttpServerConfig(config) => config.description.clone(),
    }
}

fn apply_patch(config: &McpServerConfig, patch: &McpServerConfigPatch) -> McpServerConfig {
    let mut config = config.clone();
    match &mut config {
        McpServerConfig::McpStdioServerConfig(config) => {
            if patch.enabled.is_some() {
                config.enabled = patch.enabled;
            }
            if patch.exposure.is_some() {
                config.exposure = patch.exposure;
            }
        }
        McpServerConfig::McpHttpServerConfig(config) => {
            if patch.enabled.is_some() {
                config.enabled = patch.enabled;
            }
            if patch.exposure.is_some() {
                config.exposure = patch.exposure;
            }
        }
    }
    config
}

fn is_enabled(entry: &McpServerEntry) -> bool {
    config_enabled(&entry.config) != Some(false)
}

fn exposure_of(entry: &McpServerEntry) -> McpExposure {
    entry.config.exposure().unwrap_or(McpExposure::Codemode)
}

/// Exposures the server's tools can have, known from its config before it connects.
fn configured_exposures(entry: &McpServerEntry) -> Vec<McpExposure> {
    let mut exposures = vec![exposure_of(entry)];
    for exposure in entry.config.tool_exposure().into_iter().flat_map(|map| map.values()) {
        if !exposures.contains(exposure) {
            exposures.push(*exposure);
        }
    }
    exposures
}

/// Whether some of the server's tools are declared to the model, so the first prompt waits for them.
fn has_direct_tools(entry: &McpServerEntry) -> bool {
    configured_exposures(entry).contains(&McpExposure::Direct)
}

/// Whether some of the server's tools are reached through codemode or tool_search.
fn has_indirect_tools(entry: &McpServerEntry) -> bool {
    let exposures = configured_exposures(entry);
    exposures.contains(&McpExposure::Codemode) || exposures.contains(&McpExposure::Deferred)
}

/// The section's first line. It explains only the ways of reaching tools that the listed servers use.
fn servers_section_intro(reaches: &[&str]) -> String {
    let mut intro = "MCP servers whose tools are not declared to you.".to_string();
    if reaches.contains(&"codemode") {
        intro.push_str(" Call the tools of `codemode` servers from codemode scripts.");
    }
    if reaches.contains(&"tool_search") {
        intro.push_str(" Load the tools of `tool_search` servers with `tool_search`.");
    }
    intro
}

fn truncate(text: &str, max: i64) -> String {
    if js_len(text) <= max {
        return text.to_string();
    }
    if max <= 1 {
        return String::new();
    }
    let mut kept = String::new();
    let mut units = 0;
    for c in text.chars() {
        units += c.len_utf16() as i64;
        if units > max - 1 {
            break;
        }
        kept.push(c);
    }
    format!("{}…", kept.trim_end())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerListingConnection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// What the `mcp_servers` section needs of a server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerListing {
    pub entry: McpServerEntry,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<McpServerListingConnection>,
}

/// First line of the configured description, or of the server instructions once connected.
fn server_summary(server: &McpServerListing) -> String {
    let description = config_description(&server.entry.config)
        .map(|description| description.trim().to_string())
        .filter(|description| !description.is_empty());
    let text = description
        .or_else(|| {
            server
                .connection
                .as_ref()
                .and_then(|connection| connection.instructions.clone())
        })
        .unwrap_or_default();
    first_line(&text).trim().to_string()
}

/// The `mcp_servers` section: every enabled server with codemode or deferred tools, with how its
/// tools are reached and a one-line summary. The model learns of the servers from it, since neither
/// codemode nor tool_search lists them. Undefined when there are no such servers.
pub fn render_servers_section(servers: &[McpServerListing]) -> Option<String> {
    let mut listed: Vec<&McpServerListing> = servers
        .iter()
        .filter(|server| is_enabled(&server.entry) && has_indirect_tools(&server.entry))
        .collect();
    listed.sort_by(|a, b| locale_compare(&a.entry.name, &b.entry.name));
    if listed.is_empty() {
        return None;
    }
    let reaches: Vec<&str> = listed
        .iter()
        .map(|server| {
            if configured_exposures(&server.entry).contains(&McpExposure::Codemode) {
                "codemode"
            } else {
                "tool_search"
            }
        })
        .collect();
    let intro = servers_section_intro(&reaches);
    let heads: Vec<String> = listed
        .iter()
        .zip(&reaches)
        .map(|(server, reach)| format!("- {} ({reach})", mcp_namespace(&server.entry.name)))
        .collect();
    let omitted = |count: usize| -> Vec<String> {
        if count > 0 {
            vec![format!(
                "- … {count} more server{}; find their tools with searchTools()",
                if count == 1 { "" } else { "s" }
            )]
        } else {
            Vec::new()
        }
    };
    // Characters of the intro, the first `kept` server lines without descriptions, and the omission line.
    let size = |kept: usize| -> i64 {
        let mut lines = vec![intro.clone()];
        lines.extend(heads[..kept].iter().cloned());
        lines.extend(omitted(listed.len() - kept));
        js_len(&lines.join("\n"))
    };
    let mut kept = listed.len();
    while kept > 0 && size(kept) > MAX_SERVERS_SECTION_CHARS {
        kept -= 1;
    }
    // Each description also takes a ": " separator.
    let per_server = if kept == 0 {
        0
    } else {
        MAX_SERVER_DESCRIPTION_CHARS
            .min(((MAX_SERVERS_SECTION_CHARS - size(kept)) as f64 / kept as f64).floor() as i64 - 2)
    };
    let mut lines = vec![intro.clone()];
    for (index, server) in listed[..kept].iter().enumerate() {
        let summary = if per_server > 0 {
            truncate(&server_summary(server), per_server)
        } else {
            String::new()
        };
        lines.push(if summary.is_empty() {
            heads[index].clone()
        } else {
            format!("{}: {summary}", heads[index])
        });
    }
    lines.extend(omitted(listed.len() - kept));
    Some(lines.join("\n"))
}

/// Whether a call that reaches `reach` (see `ToolDefinition.reachesTools`) needs the server: it
/// reaches every tool, or one of its words contains the server's namespace.
fn reaches_server(reach: &ReachesTools, server: &str) -> bool {
    match reach {
        ReachesTools::All(_) => true,
        ReachesTools::Words(words) => {
            let namespace = mcp_namespace(server);
            words.iter().any(|word| word.contains(&namespace))
        }
    }
}

/// Short state for lists and the startup report. `with_error` appends the first line of a failure.
fn describe_state(server: &McpServer, with_error: bool) -> String {
    if !is_enabled(&server.entry) {
        return "disabled".to_string();
    }
    let Some(connection) = &server.connection else {
        return "starting".to_string();
    };
    match connection.state() {
        ServerState::NeedsAuth => "needs sign-in".to_string(),
        ServerState::Failed => {
            if with_error {
                format!(
                    "failed: {}",
                    first_line(&connection.error().unwrap_or_else(|| "unknown error".into()))
                )
            } else {
                "failed".to_string()
            }
        }
        ServerState::Connected => {
            let tools = connection.tools().len();
            format!(
                "connected · {tools} tool{}{}",
                if tools == 1 { "" } else { "s" },
                if connection.has_resources() {
                    " · resources"
                } else {
                    ""
                }
            )
        }
        ServerState::Connecting => "connecting…".to_string(),
        other => other.as_str().to_string(),
    }
}

/// Servers that need the user first.
fn attention_rank(server: &McpServer) -> u8 {
    if !is_enabled(&server.entry) {
        return 5;
    }
    match server.connection.as_ref().map(|connection| connection.state()) {
        Some(ServerState::NeedsAuth) => 0,
        Some(ServerState::Failed) => 1,
        Some(ServerState::Disconnected) => 2,
        Some(ServerState::Connected) => 4,
        _ => 3,
    }
}

fn describe_transport(entry: &McpServerEntry) -> String {
    match &entry.config {
        McpServerConfig::McpHttpServerConfig(config) => config.url.clone(),
        McpServerConfig::McpStdioServerConfig(config) => {
            let mut parts = vec![config.command.clone()];
            parts.extend(config.args.iter().flatten().cloned());
            parts.join(" ")
        }
    }
}

const MCP_USAGE: &str =
    "Usage: /mcp, /mcp login [server], /mcp logout [server], /mcp reconnect [server]";

fn no_servers_message() -> String {
    let path = Path::new(&get_agent_dir()).join("mcp.json");
    format!(
        "No MCP servers configured. Add them to {} or .pi/mcp.json.",
        path.to_string_lossy()
    )
}

/// A token cancelled when either `a` or `b` is.
fn any_token(a: &CancellationToken, b: &CancellationToken) -> CancellationToken {
    let combined = a.child_token();
    if b.is_cancelled() {
        combined.cancel();
        return combined;
    }
    let (b, watcher) = (b.clone(), combined.clone());
    crate::utils::uncaught::spawn(async move {
        tokio::select! {
            _ = b.cancelled() => watcher.cancel(),
            _ = watcher.cancelled() => {}
        }
    });
    combined
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

struct ExtState {
    servers: Vec<ServerRef>,
    /// Servers from `mcp.json`, which take precedence over registered servers of the same name.
    configured_entries: Vec<McpServerEntry>,
    config_errors: Vec<String>,
    /// The trusted project's `mcp.json`, where `/mcp` saves project overrides of global servers.
    project_config: Option<String>,
    /// Registered servers that `mcp.json` overrides, shown in `/mcp`.
    overridden: Vec<String>,
    auto_enable_codemode: bool,
    /// Whether the "codemode tools unreachable" warning was shown since the session started.
    warned_unreachable: bool,
    pending: Option<McpReady>,
    /// Whether a prompt already waited for the startup connections since the session started.
    waited_for_startup: bool,
    /// Working directory of the session, for stdio servers.
    session_cwd: String,
    credentials: Option<Arc<McpOAuthCredentialStore>>,
    /// The session's context host, whose model registry resolves `auth.provider` tokens.
    host: Option<Arc<dyn ExtensionContextHost>>,
    server_log: Option<Arc<McpServerLog>>,
    /// Lifetime of the current session: cancelled on session_shutdown, and before the first
    /// session_start (registrations before that are read on session_start). Work captures the token
    /// when it starts and drops late results once it is cancelled.
    session: CancellationToken,
    /// pi tool name to the `<server>\0<tool>` it was assigned to, so names stay unique and stable.
    tool_owners: HashMap<String, String>,
    /// Tool names currently offered by each server.
    server_tools: HashMap<String, Vec<String>>,
    /// Last definition registered under each tool name, to re-register withdrawn tools as hidden.
    definitions: HashMap<String, ToolDefinition>,
    /// Exposure the resource tools were last registered with; undefined until a server has resources.
    resource_tools_exposure: Option<McpExposure>,
    /// Stored tokens of servers waiting for a sign-in, as they were when the sign-in was needed.
    /// `pi mcp login` in another process (for example run by the agent) changes them.
    tokens_at_sign_in: Vec<(Arc<McpServerConnection>, String)>,
}

/// The MCP extension instance of one runtime (TS: the closure state of `createMcpExtension`).
struct McpExtension {
    this: Weak<McpExtension>,
    pi: ExtensionAPIRef,
    options: McpExtensionOptions,
    startup_wait_ms: i64,
    open_url: Arc<dyn Fn(String) + Send + Sync>,
    update_config: McpUpdateConfigFn,
    state: Mutex<ExtState>,
    listeners: Mutex<Vec<(u64, Arc<dyn Fn() + Send + Sync>)>>,
    /// Work registered with `track`: manager actions opening or closing connections, and sign-ins.
    background: Mutex<HashMap<u64, McpReady>>,
}

fn ptr_in(servers: &[ServerRef], server: &ServerRef) -> bool {
    servers.iter().any(|candidate| Arc::ptr_eq(candidate, server))
}

fn ready_future(future: impl std::future::Future<Output = ()> + Send + 'static) -> McpReady {
    let shared = future.boxed().shared();
    // Promises run whether or not anyone awaits them.
    crate::utils::uncaught::spawn(shared.clone());
    shared
}

/// Sign-in prompt of the manager view: shows the URL with a copy key and cancels with Esc.
struct UiSignInPrompt {
    ui: Arc<dyn McpUi>,
    title: String,
    authorization_url: Mutex<String>,
    open_url: Arc<dyn Fn(String) + Send + Sync>,
    cancel: CancellationToken,
}

impl UiSignInPrompt {
    fn status(&self, message: &str) {
        let cancel = self.cancel.clone();
        self.ui.status(
            &self.title,
            message,
            Some(Arc::new(move || cancel.cancel())),
        );
    }
}

#[async_trait::async_trait]
impl McpSignInPrompt for UiSignInPrompt {
    fn show_authorization_url(&self, url: &str) {
        *self.authorization_url.lock() = url.to_string();
        (self.open_url)(url.to_string());
    }

    async fn prompt_for_redirect_url(
        &self,
        signal: CancellationToken,
    ) -> anyhow::Result<Option<String>> {
        let url = self.authorization_url.lock().clone();
        let value = self.ui.redirect_url(&self.title, &url, signal).await?;
        self.status("Connecting…");
        Ok(value)
    }
}

/// Sign-in prompt of RPC mode: the URL as a notification and the redirect URL from an input dialog.
struct DialogSignInPrompt {
    ctx: ExtensionCommandContext,
    name: String,
    open_url: Arc<dyn Fn(String) + Send + Sync>,
}

#[async_trait::async_trait]
impl McpSignInPrompt for DialogSignInPrompt {
    fn show_authorization_url(&self, url: &str) {
        if let Ok(ui) = self.ctx.ui() {
            ui.notify(
                &format!(
                    "Sign in to MCP server \"{}\" in your browser:\n{url}",
                    self.name
                ),
                NotifyType::Info,
            );
        }
        (self.open_url)(url.to_string());
    }

    async fn prompt_for_redirect_url(
        &self,
        signal: CancellationToken,
    ) -> anyhow::Result<Option<String>> {
        let ui = self.ctx.ui()?;
        Ok(ui
            .input(
                &format!(
                    "Waiting for sign-in to \"{}\". If the browser cannot reach this machine, paste the URL it was redirected to.",
                    self.name
                ),
                Some("http://127.0.0.1:.../callback?code=..."),
                ExtensionUIDialogOptions {
                    signal: Some(signal),
                    timeout: None,
                },
            )
            .await)
    }
}

struct PickOptions {
    eligible: fn(&McpServer) -> bool,
    preferred: fn(&McpServer) -> bool,
    none: &'static str,
}

fn server_uses_oauth(server: &McpServer) -> bool {
    server
        .connection
        .as_ref()
        .is_some_and(|connection| connection.oauth_url().is_some())
}

const OAUTH_PICK: PickOptions = PickOptions {
    eligible: server_uses_oauth,
    preferred: |server| {
        server
            .connection
            .as_ref()
            .is_some_and(|connection| connection.state() == ServerState::NeedsAuth)
    },
    none: "No enabled MCP server uses OAuth. Only HTTP servers without an Authorization header do.",
};

const RECONNECT_PICK: PickOptions = PickOptions {
    eligible: |server| server.connection.is_some(),
    preferred: |server| {
        server.connection.as_ref().is_some_and(|connection| {
            matches!(
                connection.state(),
                ServerState::Failed | ServerState::Disconnected
            )
        })
    },
    none: "No enabled MCP server to reconnect.",
};

static MCP_TOOL_NAME: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^mcp__(.+?)__(.+)$").expect("valid regex"));
static WHITESPACE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\s+").expect("valid regex"));

fn notify(ctx: &ExtensionContext, message: &str, kind: NotifyType) {
    if let Ok(ui) = ctx.ui() {
        ui.notify(message, kind);
    }
}

impl McpExtension {
    fn arc(&self) -> Arc<McpExtension> {
        self.this.upgrade().expect("MCP extension is alive")
    }

    fn servers(&self) -> Vec<ServerRef> {
        self.state.lock().servers.clone()
    }

    fn session(&self) -> CancellationToken {
        self.state.lock().session.clone()
    }

    fn emit_change(&self) {
        let listeners: Vec<_> = self
            .listeners
            .lock()
            .iter()
            .map(|(_, listener)| listener.clone())
            .collect();
        for listener in listeners {
            listener();
        }
    }

    fn subscribe(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Arc<dyn Fn() + Send + Sync> {
        let id = next_id();
        self.listeners.lock().push((id, listener));
        let this = self.this.clone();
        Arc::new(move || {
            if let Some(this) = this.upgrade() {
                this.listeners.lock().retain(|(entry, _)| *entry != id);
            }
        })
    }

    fn subscriber(&self) -> McpMenuSubscribe {
        let this = self.this.clone();
        Arc::new(move |listener| match this.upgrade() {
            Some(this) => this.subscribe(listener),
            None => Arc::new(|| {}),
        })
    }

    /// Run `work` to completion even when nobody waits, and let shutdown wait for it.
    fn track<T: Send + 'static>(
        &self,
        work: impl std::future::Future<Output = T> + Send + 'static,
    ) -> tokio::task::JoinHandle<T> {
        let id = next_id();
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        let done: McpReady = async move {
            let _ = receiver.await;
        }
        .boxed()
        .shared();
        self.background.lock().insert(id, done);
        let this = self.this.clone();
        crate::utils::uncaught::spawn(async move {
            let result = work.await;
            if let Some(this) = this.upgrade() {
                this.background.lock().remove(&id);
            }
            let _ = sender.send(());
            result
        })
    }


    fn find_server(&self, name: &str) -> Option<ServerRef> {
        self.servers()
            .into_iter()
            .find(|server| server.lock().entry.name == name)
    }

    /// Servers extensions registered, except names `mcp.json` defines, which take precedence.
    fn registered_servers(&self) -> (Vec<ServerRef>, Vec<String>) {
        let configured = self.state.lock().configured_entries.clone();
        let mut registered = Vec::new();
        let mut overridden = Vec::new();
        for server in self.pi.get_mcp_servers().unwrap_or_default() {
            let namespace = mcp_namespace(&server.name);
            if let Some(entry) = configured
                .iter()
                .find(|entry| mcp_namespace(&entry.name) == namespace)
            {
                overridden.push(format!(
                    "\"{}\" registered by {} is overridden by \"{}\" in {}",
                    server.name, server.extension_path, entry.name, entry.source
                ));
                continue;
            }
            let mut slot = McpServer::new(McpServerEntry {
                name: server.name.clone(),
                config: server.config.clone(),
                source: server.extension_path.clone(),
                scope: Some(McpServerEntryScope::Extension),
                r#override: None,
            });
            slot.registered_config = serde_json::to_string(&server.config).ok();
            registered.push(Arc::new(Mutex::new(slot)));
        }
        (registered, overridden)
    }

    fn get_credentials(&self) -> Arc<McpOAuthCredentialStore> {
        let mut state = self.state.lock();
        state
            .credentials
            .get_or_insert_with(|| Arc::new(McpOAuthCredentialStore::new(None, None)))
            .clone()
    }

    fn get_server_log(&self) -> Arc<McpServerLog> {
        let mut state = self.state.lock();
        let path = self.options.log_path.clone().unwrap_or_else(|| {
            Path::new(&get_agent_dir())
                .join("mcp.log")
                .to_string_lossy()
                .into_owned()
        });
        state
            .server_log
            .get_or_insert_with(|| Arc::new(McpServerLog::new(&path)))
            .clone()
    }

    fn register_tool(&self, definition: ToolDefinition) {
        let _ = self.pi.register_tool(definition);
    }

    fn register_tools(&self, connection: &Arc<McpServerConnection>) {
        let server = connection.name();
        let entry = self
            .find_server(&server)
            .map(|found| found.lock().entry.clone())
            .unwrap_or_else(|| connection.entry());
        let description = config_description(&entry.config)
            .map(|description| description.trim().to_string())
            .filter(|description| !description.is_empty());
        let namespace = ToolNamespace {
            name: mcp_namespace(&server),
            // `describeNamespace("docs")` finds `mcp__docs`.
            aliases: Some(vec![server.clone()]),
            description,
            instructions: connection.instructions(),
        };
        let tools = connection.tools();
        let timeout_ms = connection.timeout_ms();
        let mut to_register = Vec::new();
        {
            let mut state = self.state.lock();
            let previous = state.server_tools.get(&server).cloned().unwrap_or_default();
            let mut current: Vec<String> = Vec::new();
            // Like Codex, all tools whose names sanitize to the same name get the hash suffix, so which
            // one would keep the plain name does not depend on the order of the list.
            let mut unique: Vec<&str> = Vec::new();
            for tool in &tools {
                if !unique.contains(&tool.name.as_str()) {
                    unique.push(&tool.name);
                }
            }
            let plain: Vec<String> = unique
                .iter()
                .map(|tool| create_mcp_tool_name(&server, tool, None))
                .collect();
            for tool in &tools {
                let owner = format!("{server}\0{}", tool.name);
                let name = create_mcp_tool_name_with(&server, &tool.name, |candidate| {
                    state
                        .tool_owners
                        .get(candidate)
                        .is_some_and(|existing| *existing != owner)
                        || current.iter().any(|name| name == candidate)
                        || plain.iter().filter(|name| *name == candidate).count() > 1
                });
                state.tool_owners.insert(name.clone(), owner);
                if !current.contains(&name) {
                    current.push(name.clone());
                }
                let this = self.this.clone();
                let get_server = server.clone();
                let get_tool = tool.name.clone();
                let readable_this = self.this.clone();
                let readable_server = server.clone();
                let definition = create_mcp_tool_definition(&CreateMcpToolDefinitionOptions {
                    server: server.clone(),
                    tool: tool.clone(),
                    name: name.clone(),
                    exposure: get_mcp_tool_exposure(&entry.config, &tool.name),
                    namespace: namespace.clone(),
                    timeout_ms,
                    get_client: Arc::new(move || {
                        let this = this.clone();
                        let server = get_server.clone();
                        let tool = get_tool.clone();
                        Box::pin(async move {
                            let Some(this) = this.upgrade() else {
                                anyhow::bail!("MCP server \"{server}\" is disabled.");
                            };
                            this.tool_client(&server, &tool)
                        })
                    }),
                    readable_resources: Some(Arc::new(move || {
                        readable_this.upgrade().is_some_and(|this| {
                            this.resource_servers()
                                .iter()
                                .any(|current| current.name() == readable_server)
                        })
                    })),
                });
                state.definitions.insert(name, definition.clone());
                to_register.push(definition);
            }
            // Tools cannot be unregistered, so tools the server dropped are re-registered as hidden. When
            // the server offers them again they are registered with their configured exposure above.
            for name in &previous {
                if !current.contains(name)
                    && let Some(definition) = state.definitions.get(name)
                {
                    let mut hidden = definition.clone();
                    hidden.exposure = Some(ToolExposure::Hidden);
                    to_register.push(hidden);
                }
            }
            state.server_tools.insert(server.clone(), current);
        }
        for definition in to_register {
            self.register_tool(definition);
        }
        self.sync_resource_tools();
    }

    /// The client a prepared call runs on. A prepared call can outlive its definition's connection
    /// while the readiness hook waits for a disable/re-enable, so the replacement is resolved here.
    fn tool_client(&self, server: &str, tool: &str) -> anyhow::Result<Arc<dyn McpToolCaller>> {
        let Some(current) = self.find_server(server) else {
            anyhow::bail!("MCP server \"{server}\" is disabled.");
        };
        let (entry, connection) = {
            let current = current.lock();
            (current.entry.clone(), current.connection.clone())
        };
        if !is_enabled(&entry) {
            anyhow::bail!("MCP server \"{server}\" is disabled.");
        }
        let Some(client) = connection else {
            anyhow::bail!("MCP server \"{server}\" is still starting.");
        };
        if get_mcp_tool_exposure(&entry.config, tool) == McpExposure::Hidden
            || (client.state() == ServerState::Connected
                && !client.tools().iter().any(|offered| offered.name == tool))
        {
            anyhow::bail!("MCP tool \"{server}/{tool}\" is no longer available.");
        }
        Ok(client)
    }

    /// Make a disabled server's tools unreachable.
    fn hide_tools(&self, server: &str) {
        let hidden: Vec<ToolDefinition> = {
            let mut state = self.state.lock();
            let names = state.server_tools.insert(server.to_string(), Vec::new());
            names
                .unwrap_or_default()
                .iter()
                .filter_map(|name| state.definitions.get(name))
                .map(|definition| {
                    let mut hidden = definition.clone();
                    hidden.exposure = Some(ToolExposure::Hidden);
                    hidden
                })
                .collect()
        };
        for definition in hidden {
            self.register_tool(definition);
        }
        self.sync_resource_tools();
    }

    /// Enabled servers with resources whose exposure is not `hidden`, which the resource tools reach.
    fn servers_with_resources(&self) -> Vec<(McpServerEntry, Arc<McpServerConnection>)> {
        self.servers()
            .iter()
            .filter_map(|server| {
                let server = server.lock();
                let connection = server.connection.clone()?;
                (connection.has_resources()
                    && is_enabled(&server.entry)
                    && exposure_of(&server.entry) != McpExposure::Hidden)
                    .then(|| (server.entry.clone(), connection))
            })
            .collect()
    }

    fn resource_servers(&self) -> Vec<Arc<McpServerConnection>> {
        self.servers_with_resources()
            .into_iter()
            .map(|(_, connection)| connection)
            .collect()
    }

    /// Register the resource tools with the widest exposure of the servers they reach: `direct` when
    /// one of them is direct, and so on. They are hidden when no server has resources.
    fn sync_resource_tools(&self) {
        let exposures: Vec<McpExposure> = self
            .servers_with_resources()
            .iter()
            .map(|(entry, _)| exposure_of(entry))
            .collect();
        let next = [
            McpExposure::Direct,
            McpExposure::Codemode,
            McpExposure::Deferred,
        ]
        .into_iter()
        .find(|candidate| exposures.contains(candidate))
        .unwrap_or(McpExposure::Hidden);
        let was_direct = {
            let mut state = self.state.lock();
            let current = state.resource_tools_exposure;
            if current == Some(next) || (current.is_none() && next == McpExposure::Hidden) {
                return;
            }
            state.resource_tools_exposure = Some(next);
            current == Some(McpExposure::Direct)
        };
        let this = self.this.clone();
        let definitions = create_mcp_resource_tool_definitions(&CreateMcpResourceToolDefinitionsOptions {
            exposure: next,
            servers: Arc::new(move || {
                this.upgrade()
                    .map(|this| {
                        this.resource_servers()
                            .into_iter()
                            .map(|connection| connection as Arc<dyn McpResourceServer>)
                            .collect()
                    })
                    .unwrap_or_default()
            }),
        })
        .unwrap_or_default();
        let names: Vec<String> = definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect();
        for definition in definitions {
            self.register_tool(definition);
        }
        if was_direct {
            let active = self.pi.get_active_tools().unwrap_or_default();
            let _ = self.pi.set_active_tools(
                active
                    .into_iter()
                    .filter(|name| !names.contains(name))
                    .collect(),
            );
        }
    }

    /// Tools that are not declared to the model are reached through the codemode tool (scripts call
    /// them) or the tool_search tool (it declares them). Either reaches every such tool. Activate the
    /// one the tools' exposure asks for: codemode for `codemode` unless `autoEnableCodemode` is false,
    /// tool_search for `deferred`.
    fn ensure_discovery_active(&self, ctx: &ExtensionContext) {
        // From the config, so the tool is active before the servers connect. Resource tools share
        // their server's exposure.
        let mut exposures: Vec<McpExposure> = Vec::new();
        for server in self.servers() {
            let entry = server.lock().entry.clone();
            if is_enabled(&entry) {
                exposures.extend(configured_exposures(&entry));
            }
        }
        let needs_codemode = exposures.contains(&McpExposure::Codemode);
        let needs_tool_search = exposures.contains(&McpExposure::Deferred);
        if !needs_codemode && !needs_tool_search {
            return;
        }
        // Only a tool of these names that declares `reachesTools` can reach MCP tools; another
        // extension's tool of the same name without it is never activated.
        let tools = self.pi.get_all_tools().unwrap_or_default();
        let has_reaching_tool = |name: &str| {
            tools
                .iter()
                .any(|tool| tool.name == name && tool.reaches_tools.is_some())
        };
        let has_codemode = has_reaching_tool(CODEMODE_TOOL_NAME);
        let has_tool_search = has_reaching_tool(TOOL_SEARCH_TOOL_NAME);
        let active = self.pi.get_active_tools().unwrap_or_default();
        let auto_enable_codemode = self.state.lock().auto_enable_codemode;
        let mut activate: Vec<String> = Vec::new();
        if needs_codemode
            && has_codemode
            && auto_enable_codemode
            && !active.iter().any(|name| name == CODEMODE_TOOL_NAME)
        {
            activate.push(CODEMODE_TOOL_NAME.to_string());
        }
        if needs_tool_search
            && has_tool_search
            && !active.iter().any(|name| name == TOOL_SEARCH_TOOL_NAME)
        {
            activate.push(TOOL_SEARCH_TOOL_NAME.to_string());
        }
        let mut reachable = active.clone();
        reachable.extend(activate.iter().cloned());
        if !activate.is_empty() {
            let _ = self.pi.set_active_tools(reachable.clone());
        }
        if has_codemode && reachable.iter().any(|name| name == CODEMODE_TOOL_NAME) {
            return;
        }
        if has_tool_search && reachable.iter().any(|name| name == TOOL_SEARCH_TOOL_NAME) {
            return;
        }
        {
            let mut state = self.state.lock();
            if state.warned_unreachable {
                return;
            }
            state.warned_unreachable = true;
        }
        let reason = if needs_codemode && has_codemode && !auto_enable_codemode {
            " (autoEnableCodemode is false)"
        } else {
            ""
        };
        notify(
            ctx,
            &format!(
                "MCP tools are only reachable from the codemode or tool_search tool, but neither is active{reason}; they cannot be called."
            ),
            NotifyType::Warning,
        );
    }

    fn stored_tokens(&self, connection: &McpServerConnection) -> String {
        let credentials = self.state.lock().credentials.clone();
        match (connection.oauth_url(), credentials) {
            (Some(url), Some(credentials)) => serde_json::to_string(
                &credentials
                    .tokens(&connection.name(), &url)
                    .map(|tokens| serde_json::to_value(tokens).unwrap_or_default()),
            )
            .unwrap_or_else(|_| "null".into()),
            _ => "null".to_string(),
        }
    }

    fn on_connection_change(&self, connection: &Arc<McpServerConnection>) {
        if connection.state() != ServerState::NeedsAuth {
            self.state
                .lock()
                .tokens_at_sign_in
                .retain(|(candidate, _)| !Arc::ptr_eq(candidate, connection));
        } else {
            let known = self
                .state
                .lock()
                .tokens_at_sign_in
                .iter()
                .any(|(candidate, _)| Arc::ptr_eq(candidate, connection));
            if !known {
                let tokens = self.stored_tokens(connection);
                self.state
                    .lock()
                    .tokens_at_sign_in
                    .push((connection.clone(), tokens));
            }
        }
        self.emit_change();
    }

    /// Reconnect servers that need a sign-in when their credentials were stored since.
    async fn reconnect_signed_in(&self, ctx: &ExtensionContext) {
        let waiting = self.state.lock().tokens_at_sign_in.clone();
        let signed_in: Vec<Arc<McpServerConnection>> = waiting
            .into_iter()
            .filter(|(connection, tokens)| self.stored_tokens(connection) != *tokens)
            .map(|(connection, _)| connection)
            .collect();
        if signed_in.is_empty() {
            return;
        }
        self.state.lock().tokens_at_sign_in.retain(|(candidate, _)| {
            !signed_in
                .iter()
                .any(|connection| Arc::ptr_eq(candidate, connection))
        });
        futures::future::join_all(signed_in.iter().map(|connection| connection.reconnect())).await;
        self.ensure_discovery_active(ctx);
    }

    /// Create the server's connection.
    fn create_connection(
        &self,
        server: &ServerRef,
        is_current: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Option<Arc<McpServerConnection>> {
        if !is_current() {
            return None;
        }
        let (cwd, host) = {
            let state = self.state.lock();
            (state.session_cwd.clone(), state.host.clone())
        };
        let entry = server.lock().entry.clone();
        let on_tools_this = self.this.clone();
        let on_tools_current = is_current.clone();
        let on_change_this = self.this.clone();
        let on_change_current = is_current.clone();
        let connection = McpServerConnection::new(&McpServerConnectionOptions {
            entry,
            cwd,
            create_transport: self
                .options
                .create_transport
                .clone()
                .unwrap_or_else(default_transport_factory),
            credentials: self.get_credentials(),
            provider_token: Some(Arc::new(move |provider: String| {
                let host = host.clone();
                Box::pin(async move {
                    match host.and_then(|host| host.model_registry()) {
                        Some(registry) => Ok(registry.get_api_key_for_provider(&provider).await),
                        None => Ok(None),
                    }
                })
            })),
            log: Some(self.get_server_log()),
            on_tools: Arc::new(move |connection| {
                if on_tools_current()
                    && let Some(this) = on_tools_this.upgrade()
                {
                    this.register_tools(&connection);
                }
            }),
            on_change: Some(Arc::new(move |connection| {
                let Some(this) = on_change_this.upgrade() else {
                    return;
                };
                if on_change_current() {
                    this.on_connection_change(&connection);
                } else {
                    this.state
                        .lock()
                        .tokens_at_sign_in
                        .retain(|(candidate, _)| !Arc::ptr_eq(candidate, &connection));
                }
            })),
        });
        server.lock().connection = Some(connection.clone());
        self.emit_change();
        Some(connection)
    }

    /// Connect the server in the background. The returned readiness settles when it connected or
    /// failed; failures show in its state. It starts after `after`, unless disabled, replaced, or shut
    /// down meanwhile.
    fn start_connection(&self, server: &ServerRef, after: Option<McpReady>) -> McpReady {
        let signal = self.session();
        let attempt = next_id();
        server.lock().attempt = Some(attempt);
        let is_current: Arc<dyn Fn() -> bool + Send + Sync> = {
            let this = self.this.clone();
            let server = server.clone();
            Arc::new(move || {
                let Some(this) = this.upgrade() else {
                    return false;
                };
                if signal.is_cancelled() {
                    return false;
                }
                {
                    let server = server.lock();
                    if server.attempt != Some(attempt) || !is_enabled(&server.entry) {
                        return false;
                    }
                }
                ptr_in(&this.state.lock().servers, &server)
            })
        };
        let this = self.this.clone();
        let target = server.clone();
        let ready = ready_future(async move {
            if let Some(after) = after {
                after.await;
            } else {
                tokio::task::yield_now().await;
            }
            if !is_current() {
                return;
            }
            let Some(this) = this.upgrade() else {
                return;
            };
            let Some(connection) = this.create_connection(&target, is_current.clone()) else {
                return;
            };
            if !is_current() {
                return;
            }
            let _ = connection.get_client().await;
        });
        server.lock().ready = Some(ready.clone());
        ready
    }

    /// Wait for the latest connection attempts, including ones queued while waiting, or an abort.
    async fn wait_for_servers(&self, waiting: &[ServerRef], signal: Option<CancellationToken>) {
        loop {
            let servers = self.servers();
            let ready: Vec<(ServerRef, McpReady)> = waiting
                .iter()
                .filter_map(|server| {
                    let slot = server.lock();
                    (is_enabled(&slot.entry) && ptr_in(&servers, server))
                        .then(|| slot.ready.clone())
                        .flatten()
                        .map(|ready| (server.clone(), ready))
                })
                .collect();
            if ready.is_empty() || signal.as_ref().is_some_and(CancellationToken::is_cancelled) {
                return;
            }
            let all = futures::future::join_all(ready.iter().map(|(_, ready)| ready.clone()));
            match &signal {
                Some(signal) => {
                    tokio::select! {
                        _ = all => {}
                        _ = signal.cancelled() => {}
                    }
                }
                None => {
                    all.await;
                }
            }
            if signal.as_ref().is_some_and(CancellationToken::is_cancelled) {
                return;
            }
            let servers = self.servers();
            let settled = ready.iter().all(|(server, ready)| {
                let slot = server.lock();
                !is_enabled(&slot.entry)
                    || !ptr_in(&servers, server)
                    || slot
                        .ready
                        .as_ref()
                        .is_some_and(|current| current.ptr_eq(ready))
            });
            if settled {
                return;
            }
        }
    }

    /// One message for everything that needs the user after startup, or only for `only`, servers
    /// that connected later.
    fn report_problems(&self, ctx: &ExtensionContext, only: Option<&[ServerRef]>) {
        let mut lines: Vec<String> = match only {
            Some(_) => Vec::new(),
            None => self
                .state
                .lock()
                .config_errors
                .iter()
                .map(|error| format!("config: {error}"))
                .collect(),
        };
        let servers = match only {
            Some(only) => only.to_vec(),
            None => self.servers(),
        };
        for server in servers {
            let server = server.lock().clone();
            let state = server.connection.as_ref().map(|connection| connection.state());
            if matches!(state, Some(ServerState::NeedsAuth | ServerState::Failed)) {
                lines.push(format!(
                    "{}: {}",
                    server.entry.name,
                    describe_state(&server, true)
                ));
            }
        }
        if lines.is_empty() {
            return;
        }
        notify(
            ctx,
            &format!(
                "MCP servers need attention:\n{}\nRun /mcp to fix.",
                lines
                    .iter()
                    .map(|line| format!("  {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            NotifyType::Warning,
        );
    }

    /// Save a config change; returns an error message when the file could not be updated. Changes to
    /// registered servers only apply to the current session. `in_project` adds a project override.
    fn save_config(
        &self,
        server: &ServerRef,
        patch: &McpServerConfigPatch,
        in_project: bool,
    ) -> Option<String> {
        let project_config = self.state.lock().project_config.clone();
        let mut entry = server.lock().entry.clone();
        let r#override = if in_project {
            project_config
        } else {
            entry.r#override.clone()
        };
        if let Some(r#override) = r#override {
            entry.r#override = Some(r#override);
        }
        if entry.scope != Some(McpServerEntryScope::Extension)
            && let Err(error) = (self.update_config)(entry.clone(), patch.clone())
        {
            return Some(format!(
                "Could not update {}: {error}",
                entry.r#override.as_deref().unwrap_or(&entry.source)
            ));
        }
        entry.config = apply_patch(&entry.config, patch);
        server.lock().entry = entry;
        None
    }

    /// Sign in, then reconnect. Returns an error message on failure. `cancel` or the session's end stops the sign-in.
    async fn sign_in(
        &self,
        server: &ServerRef,
        prompt: Arc<dyn McpSignInPrompt>,
        cancel: Option<CancellationToken>,
    ) -> Option<String> {
        let (name, connection) = {
            let server = server.lock();
            (server.entry.name.clone(), server.connection.clone())
        };
        let Some((connection, url)) = connection
            .and_then(|connection| connection.oauth_url().map(|url| (connection, url)))
        else {
            return Some(format!("MCP server \"{name}\" does not use OAuth."));
        };
        let session = self.session();
        let signal = match &cancel {
            Some(cancel) => any_token(&session, cancel),
            None => session.child_token(),
        };
        let settings = match connection.oauth_settings() {
            Ok(settings) => settings,
            Err(error) => return Some(format!("Sign-in failed: {error}")),
        };
        let options = crate::extensions::mcp::oauth::SignInMcpServerOptions {
            server_url: url.clone(),
            store: self.get_credentials().for_server(&name, &url),
            settings,
            challenge: connection.challenge(),
            prompt,
            signal: Some(signal),
        };
        let result = self
            .track(async move { sign_in_mcp_server(&options).await })
            .await
            .unwrap_or_else(|error| Err(anyhow::anyhow!("{error}")));
        if let Err(error) = result {
            if error.downcast_ref::<McpSignInCancelledError>().is_some() {
                return Some("Sign-in cancelled.".to_string());
            }
            return Some(format!("Sign-in failed: {error}"));
        }
        // The challenge that asked for this sign-in (for example for more scope) is answered.
        connection.set_challenge(None);
        if let Err(error) = connection.reconnect().await {
            return Some(format!("Signed in, but {error}"));
        }
        None
    }

    async fn sign_out(&self, server: &ServerRef) -> bool {
        let (name, connection) = {
            let server = server.lock();
            (server.entry.name.clone(), server.connection.clone())
        };
        let Some((connection, url)) = connection
            .and_then(|connection| connection.oauth_url().map(|url| (connection, url)))
        else {
            return false;
        };
        let removed = self.get_credentials().remove(&name, &url);
        let _ = connection.sign_out().await;
        removed
    }

    async fn reconnect(&self, server: &ServerRef) -> Option<String> {
        let (name, connection, previous) = {
            let server = server.lock();
            (
                server.entry.name.clone(),
                server.connection.clone(),
                server.ready.clone(),
            )
        };
        let Some(connection) = connection else {
            return Some(format!("MCP server \"{name}\" is disabled."));
        };
        let (sender, receiver) = tokio::sync::oneshot::channel::<Option<String>>();
        let ready = ready_future(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            let result = connection.reconnect().await;
            let _ = sender.send(result.err().map(|error| error.to_string()));
        });
        server.lock().ready = Some(ready);
        receiver.await.unwrap_or(None)
    }

    /// Returns an error message when the config could not be saved; connection errors show in the state.
    async fn set_enabled(&self, server: &ServerRef, enabled: bool, in_project: bool) -> Option<String> {
        if let Some(failed) = self.save_config(
            server,
            &McpServerConfigPatch {
                enabled: Some(enabled),
                exposure: None,
            },
            in_project,
        ) {
            return Some(failed);
        }
        if !enabled {
            let (name, connection, ready) = {
                let mut slot = server.lock();
                slot.attempt = None;
                (slot.entry.name.clone(), slot.connection.take(), slot.ready.clone())
            };
            if let Some(connection) = connection {
                // A reconnect may already have detached its old client while awaiting transport
                // shutdown. Closing the connection alone does not wait for that reconnect.
                let closing = ready_future(async move {
                    let close = async {
                        let _ = connection.close().await;
                    };
                    let ready = async {
                        if let Some(ready) = ready {
                            ready.await;
                        }
                    };
                    futures::join!(close, ready);
                });
                server.lock().closing = Some(closing);
            }
            let closing = server.lock().closing.clone();
            self.hide_tools(&name);
            self.emit_change();
            if let Some(closing) = closing {
                closing.clone().await;
                let mut slot = server.lock();
                if slot
                    .closing
                    .as_ref()
                    .is_some_and(|current| current.ptr_eq(&closing))
                {
                    slot.closing = None;
                }
            }
            return None;
        }
        let closing = server.lock().closing.clone();
        self.start_connection(server, closing).await;
        None
    }

    fn set_exposure(&self, server: &ServerRef, exposure: McpExposure) -> Option<String> {
        if let Some(failed) = self.save_config(
            server,
            &McpServerConfigPatch {
                enabled: None,
                exposure: Some(exposure),
            },
            false,
        ) {
            return Some(failed);
        }
        let (name, connection) = {
            let slot = server.lock();
            (slot.entry.name.clone(), slot.connection.clone())
        };
        if let Some(connection) = connection
            && connection.state() == ServerState::Connected
        {
            self.register_tools(&connection);
        }
        self.sync_resource_tools();
        // Tools no longer exposed directly leave the declared set; direct tools are activated on registration.
        let indirect: Vec<String> = self
            .pi
            .get_all_tools()
            .unwrap_or_default()
            .into_iter()
            .filter(|tool| tool.exposure != ToolExposure::Direct)
            .map(|tool| tool.name)
            .collect();
        let tools = self
            .state
            .lock()
            .server_tools
            .get(&name)
            .cloned()
            .unwrap_or_default();
        let active = self.pi.get_active_tools().unwrap_or_default();
        let _ = self.pi.set_active_tools(
            active
                .into_iter()
                .filter(|name| !tools.contains(name) || !indirect.contains(name))
                .collect(),
        );
        self.emit_change();
        None
    }

    // -------------------------------------------------------------------------------------------
    // Manager (`/mcp` in the TUI)
    // -------------------------------------------------------------------------------------------

    fn notices(&self) -> Vec<String> {
        let state = self.state.lock();
        state
            .config_errors
            .iter()
            .map(|error| format!("config: {error}"))
            .chain(state.overridden.iter().map(|line| format!("overridden: {line}")))
            .collect()
    }

    fn servers_menu(&self) -> McpMenu {
        let mut servers: Vec<McpServer> = self
            .servers()
            .iter()
            .map(|server| server.lock().clone())
            .collect();
        servers.sort_by(|a, b| {
            attention_rank(a)
                .cmp(&attention_rank(b))
                .then_with(|| locale_compare(&a.entry.name, &b.entry.name))
        });
        let error = self.notices().join("\n");
        McpMenu {
            title: "MCP servers".to_string(),
            details: None,
            error: (!error.is_empty()).then_some(error),
            items: servers
                .iter()
                .map(|server| {
                    let origin = if server.entry.r#override.is_some() {
                        "global, project override".to_string()
                    } else {
                        server
                            .entry
                            .scope
                            .map(|scope| scope.as_str().to_string())
                            .unwrap_or_else(|| server.entry.source.clone())
                    };
                    SelectItem {
                        value: server.entry.name.clone(),
                        label: server.entry.name.clone(),
                        description: Some(format!(
                            "{} · {} · {origin}",
                            describe_state(server, true),
                            exposure_of(&server.entry).as_str()
                        )),
                    }
                })
                .collect(),
            empty: Some(no_servers_message()),
            selected: None,
            confirm_label: "manage".to_string(),
            cancel_label: "close".to_string(),
        }
    }

    fn server_menu(&self, name: &str) -> McpMenu {
        let Some(server) = self.find_server(name) else {
            return McpMenu {
                title: name.to_string(),
                items: Vec::new(),
                empty: Some("This server is no longer configured.".to_string()),
                confirm_label: String::new(),
                cancel_label: "back".to_string(),
                ..Default::default()
            };
        };
        let server = server.lock().clone();
        let project_config = self.state.lock().project_config.clone();
        let entry = &server.entry;
        let connection = &server.connection;
        let saved = if entry.scope == Some(McpServerEntryScope::Extension) {
            "for this session".to_string()
        } else if entry.r#override.is_some() {
            "saved to the project mcp.json".to_string()
        } else if let Some(scope) = entry.scope {
            format!("saved to the {} mcp.json", scope.as_str())
        } else {
            "saved to mcp.json".to_string()
        };
        // Global servers without an override can be turned on or off for the trusted project alone.
        let in_project = entry.scope == Some(McpServerEntryScope::Global)
            && entry.r#override.is_none()
            && project_config.is_some();
        let in_project_saved = "saved to the project mcp.json".to_string();
        let item = |value: &str, label: &str, description: Option<String>| SelectItem {
            value: value.to_string(),
            label: label.to_string(),
            description,
        };
        let mut items = Vec::new();
        if !is_enabled(entry) {
            items.push(item("enable", "Enable", Some(saved.clone())));
            if in_project {
                items.push(item(
                    "enable-project",
                    "Enable in this project",
                    Some(in_project_saved.clone()),
                ));
            }
        } else {
            let state = connection.as_ref().map(|connection| connection.state());
            if state == Some(ServerState::NeedsAuth) {
                items.push(item("signin", "Sign in", Some("opens the browser".into())));
            }
            if state == Some(ServerState::Connected)
                && let Some(connection) = connection
            {
                items.push(item(
                    "tools",
                    "Tools",
                    Some(format!("{} offered", connection.tools().len())),
                ));
            }
            if matches!(
                state,
                Some(
                    ServerState::Failed
                        | ServerState::Disconnected
                        | ServerState::Connected
                        | ServerState::NeedsAuth
                )
            ) {
                items.push(item("reconnect", "Reconnect", None));
            }
            if state == Some(ServerState::Connected)
                && connection
                    .as_ref()
                    .is_some_and(|connection| connection.oauth_url().is_some())
            {
                items.push(item(
                    "signout",
                    "Sign out",
                    Some("deletes the stored credentials".into()),
                ));
            }
            items.push(item(
                "exposure",
                "Exposure",
                Some(exposure_of(entry).as_str().to_string()),
            ));
            items.push(item("disable", "Disable", Some(saved.clone())));
            if in_project {
                items.push(item(
                    "disable-project",
                    "Disable in this project",
                    Some(in_project_saved),
                ));
            }
        }
        let mut details = vec![
            describe_transport(entry),
            format!(
                "{}: {}",
                entry.scope.map(|scope| scope.as_str()).unwrap_or("config"),
                entry.source
            ),
        ];
        if let Some(r#override) = &entry.r#override {
            details.push(format!("project override: {override}"));
        }
        details.push(format!("State: {}", describe_state(&server, false)));
        let connection_error = connection.as_ref().and_then(|connection| {
            (connection.state() != ServerState::Connected)
                .then(|| connection.error())
                .flatten()
        });
        let error = [server.message.clone(), connection_error]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
        McpMenu {
            title: format!("MCP server {name}"),
            details: Some(details.join("\n")),
            error: (!error.is_empty()).then_some(error),
            selected: items.first().map(|item| item.value.clone()),
            items,
            empty: None,
            confirm_label: "select".to_string(),
            cancel_label: "back".to_string(),
        }
    }

    async fn show_tools(&self, ui: &Arc<dyn McpUi>, server: &ServerRef) {
        let server = server.clone();
        let _ = ui
            .menu(
                Arc::new(move || {
                    let server = server.lock().clone();
                    let exposure = exposure_of(&server.entry);
                    let overridden = server
                        .entry
                        .config
                        .tool_exposure()
                        .is_some_and(|map| !map.is_empty());
                    let reach = if exposure == McpExposure::Hidden {
                        "unreachable"
                    } else {
                        exposure_description(exposure)
                    };
                    McpMenu {
                        title: format!("Tools of {}", server.entry.name),
                        details: Some(format!(
                            "Exposure {}: {reach}{}",
                            exposure.as_str(),
                            if overridden {
                                "\nSome tools override it with toolExposure."
                            } else {
                                ""
                            }
                        )),
                        items: server
                            .connection
                            .as_ref()
                            .map(|connection| connection.tools())
                            .unwrap_or_default()
                            .iter()
                            .map(|tool| {
                                let tool_exposure =
                                    get_mcp_tool_exposure(&server.entry.config, &tool.name);
                                let description =
                                    first_line(tool.description.as_deref().unwrap_or(""));
                                SelectItem {
                                    value: tool.name.clone(),
                                    label: tool.name.clone(),
                                    description: Some(if tool_exposure == exposure {
                                        description
                                    } else {
                                        format!("[{}] {description}", tool_exposure.as_str())
                                    }),
                                }
                            })
                            .collect(),
                        empty: Some("The server offers no tools.".to_string()),
                        confirm_label: "back".to_string(),
                        cancel_label: "back".to_string(),
                        ..Default::default()
                    }
                }),
                None,
            )
            .await;
    }

    async fn choose_exposure(&self, ui: &Arc<dyn McpUi>, server: &ServerRef) -> Option<String> {
        let entry = server.lock().entry.clone();
        let current = exposure_of(&entry);
        let menu_entry = entry.clone();
        let choice = ui
            .menu(
                Arc::new(move || McpMenu {
                    title: format!("Exposure of {}", menu_entry.name),
                    details: Some(if menu_entry.scope == Some(McpServerEntryScope::Extension) {
                        format!(
                            "Applies to this session; the server is registered by {}.",
                            menu_entry.source
                        )
                    } else {
                        format!(
                            "Saved to {}.",
                            menu_entry.r#override.as_deref().unwrap_or(&menu_entry.source)
                        )
                    }),
                    items: EXPOSURE_DESCRIPTIONS
                        .iter()
                        .map(|(exposure, description)| SelectItem {
                            value: exposure.as_str().to_string(),
                            label: format!(
                                "{}{}",
                                if *exposure == current { "✓ " } else { "  " },
                                exposure.as_str()
                            ),
                            description: Some(description.to_string()),
                        })
                        .collect(),
                    selected: Some(current.as_str().to_string()),
                    confirm_label: "save".to_string(),
                    cancel_label: "back".to_string(),
                    ..Default::default()
                }),
                None,
            )
            .await
            .ok()
            .flatten()?;
        if choice == current.as_str() {
            return None;
        }
        let exposure = EXPOSURE_DESCRIPTIONS
            .iter()
            .map(|(exposure, _)| *exposure)
            .find(|exposure| exposure.as_str() == choice)?;
        self.set_exposure(server, exposure)
    }

    /// Sign in with the manager view's sign-in screen, which shows the URL with a copy key and cancels with Esc.
    async fn sign_in_with_ui(&self, ui: &Arc<dyn McpUi>, server: &ServerRef) -> Option<String> {
        let title = format!("Sign in to {}", server.lock().entry.name);
        let cancel = CancellationToken::new();
        let prompt = Arc::new(UiSignInPrompt {
            ui: ui.clone(),
            title,
            authorization_url: Mutex::new(String::new()),
            open_url: self.open_url.clone(),
            cancel: cancel.clone(),
        });
        prompt.status("Contacting the authorization server…");
        self.sign_in(server, prompt, Some(cancel)).await
    }

    /// Keep the subscribed menu usable while a connection opens or closes.
    fn run_in_background(
        &self,
        ctx: &ExtensionContext,
        server: &ServerRef,
        operation: BoxFuture<'static, Option<String>>,
    ) {
        let attempt = server.lock().attempt;
        let signal = self.session();
        let this = self.this.clone();
        let server = server.clone();
        let ctx = ctx.clone();
        // Detached: the tracked task runs on its own; the handle is not needed.
        drop(self.track(async move {
            let message = operation.await;
            let Some(this) = this.upgrade() else {
                return;
            };
            if signal.is_cancelled()
                || server.lock().attempt != attempt
                || !ptr_in(&this.servers(), &server)
            {
                return;
            }
            server.lock().message = message;
            this.ensure_discovery_active(&ctx);
            this.emit_change();
        }));
    }

    async fn run_action(
        &self,
        ui: &Arc<dyn McpUi>,
        ctx: &ExtensionContext,
        server: &ServerRef,
        action: &str,
    ) {
        let signal = self.session();
        let this = self.arc();
        let mut message = None;
        match action {
            "signin" => message = self.sign_in_with_ui(ui, server).await,
            "reconnect" => {
                // Connection state and error already report failures, including required sign-ins.
                let target = server.clone();
                self.run_in_background(
                    ctx,
                    server,
                    Box::pin(async move {
                        this.reconnect(&target).await;
                        None
                    }),
                );
            }
            "signout" => {
                self.sign_out(server).await;
            }
            "tools" => self.show_tools(ui, server).await,
            "exposure" => message = self.choose_exposure(ui, server).await,
            "enable" | "disable" | "enable-project" | "disable-project" => {
                let enable = action.starts_with("enable");
                let in_project = action.ends_with("-project");
                let target = server.clone();
                self.run_in_background(
                    ctx,
                    server,
                    Box::pin(async move { this.set_enabled(&target, enable, in_project).await }),
                );
            }
            _ => {}
        }
        // The session ended meanwhile (for example during a sign-in), which made ctx stale.
        if signal.is_cancelled() {
            return;
        }
        server.lock().message = message;
        self.ensure_discovery_active(ctx);
        self.emit_change();
    }

    async fn manage(&self, ui: Arc<dyn McpUi>, ctx: ExtensionContext) -> anyhow::Result<()> {
        loop {
            let this = self.this.clone();
            let Some(name) = ui
                .menu(
                    Arc::new(move || {
                        this.upgrade()
                            .map(|this| this.servers_menu())
                            .unwrap_or_default()
                    }),
                    Some(self.subscriber()),
                )
                .await?
            else {
                return Ok(());
            };
            loop {
                let this = self.this.clone();
                let menu_name = name.clone();
                let action = ui
                    .menu(
                        Arc::new(move || {
                            this.upgrade()
                                .map(|this| this.server_menu(&menu_name))
                                .unwrap_or_default()
                        }),
                        Some(self.subscriber()),
                    )
                    .await?;
                let server = self.find_server(&name);
                let (Some(action), Some(server)) = (action, server) else {
                    break;
                };
                self.run_action(&ui, &ctx, &server, &action).await;
            }
        }
    }

    // -------------------------------------------------------------------------------------------
    // Subcommands and plain status (no TUI)
    // -------------------------------------------------------------------------------------------

    fn format_status(&self) -> String {
        let (servers, config_errors, overridden) = {
            let state = self.state.lock();
            (
                state.servers.clone(),
                state.config_errors.clone(),
                state.overridden.clone(),
            )
        };
        if servers.is_empty() && config_errors.is_empty() && overridden.is_empty() {
            return no_servers_message();
        }
        let mut lines: Vec<String> = servers
            .iter()
            .map(|server| {
                let server = server.lock().clone();
                let name = &server.entry.name;
                let exposure = exposure_of(&server.entry).as_str();
                let connection = &server.connection;
                let state = connection.as_ref().map(|connection| connection.state());
                if state == Some(ServerState::NeedsAuth) {
                    return format!("{name}: needs sign-in, run /mcp login {name} ({exposure})");
                }
                let tools = match connection {
                    Some(connection) if state == Some(ServerState::Connected) => {
                        format!(", {} tools", connection.tools().len())
                    }
                    _ => String::new(),
                };
                let state_text = if !is_enabled(&server.entry) {
                    "disabled".to_string()
                } else if state == Some(ServerState::Disconnected) {
                    "disconnected, reconnects on next call".to_string()
                } else {
                    state
                        .map(|state| state.as_str().to_string())
                        .unwrap_or_else(|| "starting".to_string())
                };
                let error = match connection.as_ref().and_then(|connection| connection.error()) {
                    Some(error) if state != Some(ServerState::Connected) && !error.is_empty() => {
                        format!("\n    {}", error.split('\n').collect::<Vec<_>>().join("\n    "))
                    }
                    _ => String::new(),
                };
                format!("{name}: {state_text}{tools} ({exposure}){error}")
            })
            .collect();
        for error in config_errors {
            lines.push(format!("config error: {error}"));
        }
        for line in overridden {
            lines.push(format!("overridden: {line}"));
        }
        lines.join("\n")
    }

    /// Resolve the server for a subcommand, asking when the name is omitted and ambiguous.
    async fn pick_server(
        &self,
        name: Option<&str>,
        ctx: &ExtensionCommandContext,
        options: &PickOptions,
    ) -> Option<ServerRef> {
        if let Some(name) = name {
            let Some(server) = self.find_server(name) else {
                notify(ctx, &format!("No MCP server named \"{name}\"."), NotifyType::Error);
                return None;
            };
            let eligible = (options.eligible)(&server.lock().clone());
            if !eligible {
                notify(ctx, options.none, NotifyType::Error);
                return None;
            }
            return Some(server);
        }
        let candidates: Vec<ServerRef> = self
            .servers()
            .into_iter()
            .filter(|server| (options.eligible)(&server.lock().clone()))
            .collect();
        if candidates.is_empty() {
            notify(ctx, options.none, NotifyType::Info);
            return None;
        }
        let preferred: Vec<&ServerRef> = candidates
            .iter()
            .filter(|server| (options.preferred)(&server.lock().clone()))
            .collect();
        if candidates.len() == 1 {
            return candidates.into_iter().next();
        }
        if preferred.len() == 1 {
            return Some(preferred[0].clone());
        }
        let names: Vec<String> = candidates
            .iter()
            .map(|server| server.lock().entry.name.clone())
            .collect();
        let ui = ctx.ui().ok()?;
        let choice = ui
            .select("MCP server", names, ExtensionUIDialogOptions::default())
            .await?;
        candidates
            .into_iter()
            .find(|server| server.lock().entry.name == choice)
    }

    async fn login_command(&self, server: &ServerRef, ctx: &ExtensionCommandContext) {
        let name = server.lock().entry.name.clone();
        if !ctx.has_ui().unwrap_or(false) {
            notify(
                ctx,
                &format!("Signing in to MCP server \"{name}\" requires interactive mode."),
                NotifyType::Error,
            );
            return;
        }
        let signal = self.session();
        let failure = if ctx.mode().ok() == Some(ExtensionMode::Tui) {
            let failure: Arc<Mutex<Option<String>>> = Arc::default();
            let this = self.this.clone();
            let target = server.clone();
            let slot = failure.clone();
            let _ = show_mcp_manager(
                ctx,
                Arc::new(move |ui| {
                    let this = this.clone();
                    let target = target.clone();
                    let slot = slot.clone();
                    Box::pin(async move {
                        if let Some(this) = this.upgrade() {
                            *slot.lock() = this.sign_in_with_ui(&ui, &target).await;
                        }
                        Ok(())
                    })
                }),
            )
            .await;
            failure.lock().take()
        } else {
            self.sign_in(
                server,
                Arc::new(DialogSignInPrompt {
                    ctx: ctx.clone(),
                    name: name.clone(),
                    open_url: self.open_url.clone(),
                }),
                None,
            )
            .await
        };
        // The session ended meanwhile, which cancelled the sign-in and made ctx stale.
        if signal.is_cancelled() {
            return;
        }
        if let Some(failure) = failure {
            let kind = if failure == "Sign-in cancelled." {
                NotifyType::Info
            } else {
                NotifyType::Error
            };
            notify(ctx, &failure, kind);
            return;
        }
        self.ensure_discovery_active(ctx);
        let tools = server
            .lock()
            .connection
            .as_ref()
            .map(|connection| connection.tools().len())
            .unwrap_or(0);
        notify(
            ctx,
            &format!("Signed in to MCP server \"{name}\" ({tools} tools)."),
            NotifyType::Info,
        );
    }

    // -------------------------------------------------------------------------------------------
    // Event handlers
    // -------------------------------------------------------------------------------------------

    fn on_session_start(&self, ctx: &ExtensionContext) {
        let loaded = match &self.options.load_config {
            Some(load) => load(ctx.clone()),
            None => default_load_config(ctx),
        };
        let signal = CancellationToken::new();
        {
            let mut state = self.state.lock();
            state.config_errors = loaded.errors.clone();
            state.project_config = loaded.project_config.clone();
            state.auto_enable_codemode = loaded.auto_enable_codemode.unwrap_or(true);
            state.warned_unreachable = false;
            state.waited_for_startup = false;
            if let Ok(cwd) = ctx.cwd() {
                state.session_cwd = cwd;
            }
            state.host = Some(ctx.host().clone());
            state.session = signal.clone();
            state.configured_entries = loaded.servers.clone();
        }
        let (registered, overridden) = self.registered_servers();
        {
            let mut state = self.state.lock();
            state.overridden = overridden;
            let mut servers: Vec<ServerRef> = loaded
                .servers
                .iter()
                .map(|entry| Arc::new(Mutex::new(McpServer::new(entry.clone()))))
                .collect();
            servers.extend(registered);
            state.servers = servers;
        }
        self.emit_change();
        // Codemode or tool_search is activated from the config: the first prompt does not wait for
        // servers whose tools are not declared to the model, and scripts or searches wait for them.
        self.ensure_discovery_active(ctx);
        let enabled: Vec<ServerRef> = self
            .servers()
            .into_iter()
            .filter(|server| is_enabled(&server.lock().entry))
            .collect();
        if enabled.is_empty() {
            self.report_problems(ctx, None);
            return;
        }
        // Waiting one turn lets the first render happen before connecting.
        let after: McpReady = async {
            tokio::task::yield_now().await;
        }
        .boxed()
        .shared();
        let readies: Vec<McpReady> = enabled
            .iter()
            .map(|server| self.start_connection(server, Some(after.clone())))
            .collect();
        let this = self.this.clone();
        let ctx = ctx.clone();
        let pending = ready_future(async move {
            futures::future::join_all(readies).await;
            if !signal.is_cancelled()
                && let Some(this) = this.upgrade()
            {
                this.report_problems(&ctx, None);
            }
        });
        self.state.lock().pending = Some(pending);
    }

    /// The first prompt waits for servers whose tools are declared to the model, so they are declared
    /// in its first request, but not indefinitely: a slow or hanging server must not hold up the
    /// prompt. Other servers are waited for when a script or search needs them.
    async fn wait_for_direct_servers(&self, ctx: &ExtensionContext) {
        {
            let mut state = self.state.lock();
            if state.waited_for_startup {
                return;
            }
            state.waited_for_startup = true;
        }
        let ready: Vec<McpReady> = self
            .servers()
            .iter()
            .filter_map(|server| {
                let server = server.lock();
                (is_enabled(&server.entry) && has_direct_tools(&server.entry))
                    .then(|| server.ready.clone())
                    .flatten()
            })
            .collect();
        if ready.is_empty() {
            return;
        }
        let finished = tokio::time::timeout(
            Duration::from_millis(self.startup_wait_ms.max(0) as u64),
            futures::future::join_all(ready),
        )
        .await
        .is_ok();
        if !finished {
            notify(
                ctx,
                "MCP servers are still connecting; their tools become available once connected.",
                NotifyType::Info,
            );
        }
    }

    fn listings(&self) -> Vec<McpServerListing> {
        self.servers()
            .iter()
            .map(|server| {
                let server = server.lock();
                McpServerListing {
                    entry: server.entry.clone(),
                    connection: server.connection.as_ref().map(|connection| {
                        McpServerListingConnection {
                            instructions: connection.instructions(),
                        }
                    }),
                }
            })
            .collect()
    }

    async fn on_before_agent_start(
        &self,
        event: BeforeAgentStartEvent,
        ctx: &ExtensionContext,
    ) -> Option<BeforeAgentStartEventResult> {
        self.wait_for_direct_servers(ctx).await;
        let mut options = event.system_prompt_options;
        let section = render_servers_section(&self.listings());
        let sections = &mut options.sections;
        match section {
            Some(section) => {
                sections.insert(MCP_SERVERS_SECTION.to_string(), section);
            }
            None => {
                sections.shift_remove(MCP_SERVERS_SECTION);
            }
        }
        Some(BeforeAgentStartEventResult {
            system_prompt_options: Some(options),
            ..Default::default()
        })
    }

    async fn on_tool_call(&self, event: ToolCallEvent, ctx: &ExtensionContext) {
        let tools = self.pi.get_all_tools().unwrap_or_default();
        let Some(tool) = tools.iter().find(|candidate| candidate.name == event.tool_name) else {
            return;
        };
        // Readiness can be pending while the display state is still connected: reconnect first
        // closes the old transport, then opens a new one. Settled futures return immediately.
        let ready_servers: Vec<ServerRef> = self
            .servers()
            .into_iter()
            .filter(|server| {
                let server = server.lock();
                is_enabled(&server.entry) && server.ready.is_some()
            })
            .collect();
        if ready_servers.is_empty() {
            return;
        }
        let reach = tool
            .reaches_tools
            .as_ref()
            .map(|reaches| reaches(&event.input));
        let waiting: Vec<ServerRef> = if let Some(reach) = reach {
            ready_servers
                .into_iter()
                .filter(|server| reaches_server(&reach, &server.lock().entry.name))
                .collect()
        } else if RESOURCE_TOOL_NAMES.contains(&tool.name.as_str()) {
            ready_servers
        } else {
            let owner = self
                .state
                .lock()
                .tool_owners
                .get(&event.tool_name)
                .map(|owner| owner.split('\0').next().unwrap_or("").to_string());
            ready_servers
                .into_iter()
                .filter(|server| Some(&server.lock().entry.name) == owner.as_ref())
                .collect()
        };
        self.wait_for_servers(&waiting, ctx.signal().ok().flatten())
            .await;
    }

    async fn on_mcp_servers_change(&self, ctx: &ExtensionContext) {
        let signal = self.session();
        if signal.is_cancelled() {
            return;
        }
        let (registered, overridden) = self.registered_servers();
        let removed: Vec<ServerRef> = {
            let mut state = self.state.lock();
            state.overridden = overridden;
            // Unregistered servers and re-registered ones with a new config are dropped; the latter come back below.
            let removed: Vec<ServerRef> = state
                .servers
                .iter()
                .filter(|server| {
                    let server = server.lock();
                    server.entry.scope == Some(McpServerEntryScope::Extension)
                        && registered
                            .iter()
                            .find(|candidate| candidate.lock().entry.name == server.entry.name)
                            .map(|candidate| candidate.lock().registered_config.clone())
                            != Some(server.registered_config.clone())
                })
                .cloned()
                .collect();
            state.servers.retain(|server| !ptr_in(&removed, server));
            removed
        };
        for server in &removed {
            let name = server.lock().entry.name.clone();
            self.hide_tools(&name);
        }
        let added: Vec<ServerRef> = registered
            .into_iter()
            .filter(|server| {
                let name = server.lock().entry.name.clone();
                self.find_server(&name).is_none()
            })
            .collect();
        self.state.lock().servers.extend(added.iter().cloned());
        self.emit_change();
        self.ensure_discovery_active(ctx);
        let closing: Vec<Arc<McpServerConnection>> = removed
            .iter()
            .filter_map(|server| server.lock().connection.clone())
            .collect();
        futures::future::join_all(closing.iter().map(|connection| connection.close())).await;
        let connecting: Vec<ServerRef> = added
            .into_iter()
            .filter(|server| is_enabled(&server.lock().entry))
            .collect();
        if signal.is_cancelled() || connecting.is_empty() {
            return;
        }
        let readies: Vec<McpReady> = connecting
            .iter()
            .map(|server| self.start_connection(server, None))
            .collect();
        futures::future::join_all(readies).await;
        if signal.is_cancelled() {
            let closing: Vec<Arc<McpServerConnection>> = connecting
                .iter()
                .filter_map(|server| server.lock().connection.clone())
                .collect();
            futures::future::join_all(closing.iter().map(|connection| connection.close())).await;
            return;
        }
        self.report_problems(ctx, Some(&connecting));
    }

    async fn on_session_shutdown(&self) {
        let closing = {
            let mut state = self.state.lock();
            state.session.cancel();
            let closing: Vec<Arc<McpServerConnection>> = state
                .servers
                .iter()
                .filter_map(|server| server.lock().connection.clone())
                .collect();
            state.servers.clear();
            closing
        };
        self.emit_change();
        let background: Vec<McpReady> = self.background.lock().values().cloned().collect();
        let close = futures::future::join_all(closing.iter().map(|connection| connection.close()));
        futures::join!(futures::future::join_all(background), close);
    }

    fn argument_completions(&self, prefix: &str) -> Option<Vec<AutocompleteItem>> {
        let parts: Vec<&str> = WHITESPACE.split(prefix.trim_start()).collect();
        if parts.len() > 2 {
            return None;
        }
        let action = parts.first().copied().unwrap_or("");
        let Some(server) = parts.get(1).copied() else {
            return Some(
                ["login", "logout", "reconnect"]
                    .iter()
                    .filter(|item| item.starts_with(action))
                    .map(|item| AutocompleteItem {
                        value: format!("{item} "),
                        label: item.to_string(),
                        description: None,
                    })
                    .collect(),
            );
        };
        if action != "login" && action != "logout" && action != "reconnect" {
            return None;
        }
        let items: Vec<AutocompleteItem> = self
            .servers()
            .iter()
            .map(|server| server.lock().clone())
            .filter(|candidate| {
                if action == "reconnect" {
                    candidate.connection.is_some()
                } else {
                    server_uses_oauth(candidate)
                }
            })
            .filter(|candidate| candidate.entry.name.starts_with(server))
            .map(|candidate| AutocompleteItem {
                value: format!("{action} {}", candidate.entry.name),
                label: candidate.entry.name.clone(),
                description: Some(describe_state(&candidate, true)),
            })
            .collect();
        (!items.is_empty()).then_some(items)
    }

    async fn await_pending(&self) {
        let pending = self.state.lock().pending.clone();
        if let Some(pending) = pending {
            pending.await;
        }
    }

    async fn handle_command(&self, args: String, ctx: ExtensionCommandContext) -> anyhow::Result<()> {
        let parts: Vec<&str> = args.split_whitespace().collect();
        let Some(action) = parts.first().copied() else {
            if ctx.mode()? == ExtensionMode::Tui {
                let this = self.this.clone();
                let manage_ctx = ctx.ctx.clone();
                show_mcp_manager(
                    &ctx,
                    Arc::new(move |ui| {
                        let this = this.clone();
                        let ctx = manage_ctx.clone();
                        Box::pin(async move {
                            match this.upgrade() {
                                Some(this) => this.manage(ui, ctx).await,
                                None => Ok(()),
                            }
                        })
                    }),
                )
                .await?;
            } else {
                self.await_pending().await;
                notify(&ctx, &self.format_status(), NotifyType::Info);
            }
            return Ok(());
        };
        let name = parts.get(1).copied();
        if parts.len() > 2 {
            notify(&ctx, MCP_USAGE, NotifyType::Warning);
            return Ok(());
        }
        self.await_pending().await;
        match action {
            "login" => {
                if let Some(server) = self.pick_server(name, &ctx, &OAUTH_PICK).await {
                    self.login_command(&server, &ctx).await;
                }
            }
            "logout" => {
                let Some(server) = self.pick_server(name, &ctx, &OAUTH_PICK).await else {
                    return Ok(());
                };
                let removed = self.sign_out(&server).await;
                let name = server.lock().entry.name.clone();
                notify(
                    &ctx,
                    &if removed {
                        format!("Signed out of MCP server \"{name}\".")
                    } else {
                        format!("No stored credentials for MCP server \"{name}\".")
                    },
                    NotifyType::Info,
                );
            }
            "reconnect" => {
                let Some(server) = self.pick_server(name, &ctx, &RECONNECT_PICK).await else {
                    return Ok(());
                };
                match self.reconnect(&server).await {
                    Some(failure) => notify(&ctx, &failure, NotifyType::Error),
                    None => {
                        self.ensure_discovery_active(&ctx);
                        let server = server.lock().clone();
                        notify(
                            &ctx,
                            &format!(
                                "Reconnected to MCP server \"{}\" ({}).",
                                server.entry.name,
                                describe_state(&server, true)
                            ),
                            NotifyType::Info,
                        );
                    }
                }
            }
            _ => notify(&ctx, MCP_USAGE, NotifyType::Warning),
        }
        Ok(())
    }
}

fn default_load_config(ctx: &ExtensionContext) -> LoadedMcpConfig {
    load_mcp_config(&LoadMcpConfigOptions {
        agent_dir: get_agent_dir(),
        cwd: ctx.cwd().unwrap_or_default(),
        project_trusted: ctx.is_project_trusted().unwrap_or(false),
    })
}

fn default_update_config() -> McpUpdateConfigFn {
    Arc::new(|entry, patch| {
        update_mcp_server_config(
            entry.r#override.as_deref().unwrap_or(&entry.source),
            &entry.name,
            &patch,
            Some(&UpdateMcpServerConfigOptions {
                r#override: Some(entry.r#override.is_some()),
            }),
        )
    })
}

/// Register the extension on `pi`.
fn install(pi: ExtensionAPIRef, options: McpExtensionOptions) -> anyhow::Result<()> {
    let session = CancellationToken::new();
    session.cancel();
    let extension = Arc::new_cyclic(|this| McpExtension {
        this: this.clone(),
        pi: pi.clone(),
        startup_wait_ms: options.startup_wait_ms.unwrap_or(DEFAULT_STARTUP_WAIT_MS),
        open_url: options.open_url.clone().unwrap_or_else(|| {
            Arc::new(|url: String| crate::utils::open_browser::open_browser(&url))
        }),
        update_config: options
            .update_config
            .clone()
            .unwrap_or_else(default_update_config),
        options,
        state: Mutex::new(ExtState {
            servers: Vec::new(),
            configured_entries: Vec::new(),
            config_errors: Vec::new(),
            project_config: None,
            overridden: Vec::new(),
            auto_enable_codemode: true,
            warned_unreachable: false,
            pending: None,
            waited_for_startup: false,
            session_cwd: std::env::current_dir()
                .map(|cwd| cwd.to_string_lossy().into_owned())
                .unwrap_or_default(),
            credentials: None,
            host: None,
            server_log: None,
            session,
            tool_owners: HashMap::new(),
            server_tools: HashMap::new(),
            definitions: HashMap::new(),
            resource_tools_exposure: None,
            tokens_at_sign_in: Vec::new(),
        }),
        listeners: Mutex::new(Vec::new()),
        background: Mutex::new(HashMap::new()),
    });
    extension.state.lock().credentials = extension.options.credentials.clone();

    // A resumed session renders calls to MCP tools before their server connected, if it ever does.
    pi.register_tool_renderer(Arc::new(
        |tool_name: &str, next: &dyn Fn() -> Option<ToolRenderers>| {
            next().or_else(|| {
                MCP_TOOL_NAME.captures(tool_name).map(|captures| {
                    create_mcp_tool_renderers(&format!("{}/{}", &captures[1], &captures[2]))
                })
            })
        },
    ))?;

    let this = extension.clone();
    on_event::<SessionStartEvent, _, _>(&*pi, move |_event, ctx| {
        let this = this.clone();
        async move {
            this.on_session_start(&ctx);
            Ok(None)
        }
    })?;

    // Every prompt lists the servers in the `mcp_servers` section as they are when it starts. Pi
    // appends the section to the conversation when it changed, for example after a server connected.
    let this = extension.clone();
    on_event::<BeforeAgentStartEvent, _, _>(&*pi, move |event, ctx| {
        let this = this.clone();
        async move { Ok(this.on_before_agent_start(event, &ctx).await) }
    })?;

    // A tool that reaches other tools (`reachesTools`) waits for the servers it may reach, so their
    // tools are registered before it runs: a codemode script for the servers it names, or for every
    // server when it searches or enumerates tools, and tool_search for every server. The resource
    // tools reach every server, so they wait for all of them. Direct MCP calls wait only for their
    // own server, including a reconnect still closing its old transport.
    let this = extension.clone();
    on_event::<ToolCallEvent, _, _>(&*pi, move |event, ctx| {
        let this = this.clone();
        async move {
            this.on_tool_call(event, &ctx).await;
            Ok(None)
        }
    })?;

    // Pick up sign-ins done outside the session, such as `pi mcp login` run by the agent.
    let this = extension.clone();
    on_event::<TurnStartEvent, _, _>(&*pi, move |_event, ctx| {
        let this = this.clone();
        async move {
            if !this.state.lock().tokens_at_sign_in.is_empty() {
                this.reconnect_signed_in(&ctx).await;
            }
            Ok(None)
        }
    })?;

    // Servers registered or unregistered during the session connect or disconnect right away.
    let this = extension.clone();
    on_event::<McpServersChangeEvent, _, _>(&*pi, move |_event, ctx| {
        let this = this.clone();
        async move {
            this.on_mcp_servers_change(&ctx).await;
            Ok(None)
        }
    })?;

    let this = extension.clone();
    on_event::<SessionShutdownEvent, _, _>(&*pi, move |_event, _ctx| {
        let this = this.clone();
        async move {
            this.on_session_shutdown().await;
            Ok(None)
        }
    })?;

    let completions = extension.clone();
    let handler = extension.clone();
    pi.register_command(
        "mcp",
        CommandOptions {
            description: Some(
                "Manage MCP servers: sign in, reconnect, enable or disable, and change exposure"
                    .to_string(),
            ),
            get_argument_completions: Some(Arc::new(move |prefix: String| {
                let items = completions.argument_completions(&prefix);
                Box::pin(async move { items })
            })),
            handler: Arc::new(move |args: String, ctx: ExtensionCommandContext| {
                let this = handler.clone();
                Box::pin(async move { this.handle_command(args, ctx).await })
            }),
        },
    )?;
    Ok(())
}

pub fn create_mcp_extension(
    options: Option<&McpExtensionOptions>,
) -> anyhow::Result<ExtensionFactory> {
    let options = options.cloned().unwrap_or_default();
    Ok(Arc::new(move |pi: ExtensionAPIRef| {
        let options = options.clone();
        Box::pin(async move { install(pi, options) })
    }))
}
