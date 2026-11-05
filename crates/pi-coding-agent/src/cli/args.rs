//! Port of `packages/coding-agent/src/cli/args.ts`.

use crate::config::{APP_NAME, CONFIG_DIR_NAME, ENV_AGENT_DIR, ENV_SESSION_DIR};
use crate::core::extensions::types::{ExtensionFlag, FlagType};
use crate::core::settings_manager::get_tool_list_error;
use crate::utils::chalk;
use crate::core::settings_schema::TuiMode;
use indexmap::IndexMap;
use pi_agent::types::ThinkingLevel;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mode {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "json")]
    Json,
    #[serde(rename = "rpc")]
    Rpc,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::Rpc => "rpc",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgsListModels {
    String(String),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgsUnknownFlags {
    Bool(bool),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArgsDiagnosticsType {
    #[serde(rename = "warning")]
    Warning,
    #[serde(rename = "error")]
    Error,
}

impl ArgsDiagnosticsType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArgsDiagnostics {
    pub r#type: ArgsDiagnosticsType,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Args {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#continue: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_session: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_builtin_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_extensions: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_mcp: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub print: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_skills: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_templates: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_prompt_templates: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_themes: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_context_files: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_models: Option<ArgsListModels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_mode: Option<TuiMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbose: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_trust_override: Option<bool>,
    pub messages: Vec<String>,
    pub file_args: Vec<String>,
    /// Unknown flags (potentially extension flags) - map of flag name to value
    pub unknown_flags: IndexMap<String, ArgsUnknownFlags>,
    pub diagnostics: Vec<ArgsDiagnostics>,
}

const VALID_THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub fn is_valid_thinking_level(level: &str) -> bool {
    VALID_THINKING_LEVELS.contains(&level)
}

pub fn normalize_session_name(value: &str) -> Option<String> {
    let name = value.trim();
    if name.is_empty() { None } else { Some(name.to_string()) }
}

fn split_list(value: &str) -> Vec<String> {
    value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

fn error(message: impl Into<String>) -> ArgsDiagnostics {
    ArgsDiagnostics { r#type: ArgsDiagnosticsType::Error, message: message.into() }
}

pub fn parse_args(args: &[String]) -> Args {
    let mut result = Args::default();
    let has_next = |i: usize| i + 1 < args.len();

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();

        if arg == "--" {
            for positional in &args[i + 1..] {
                if let Some(file) = positional.strip_prefix('@') {
                    result.file_args.push(file.to_string());
                } else {
                    result.messages.push(positional.clone());
                }
            }
            break;
        } else if arg == "--help" || arg == "-h" {
            result.help = Some(true);
        } else if arg == "--version" || arg == "-v" {
            result.version = Some(true);
        } else if arg == "--mode" {
            match args.get(i + 1) {
                None => result.diagnostics.push(error("--mode requires text, json, or rpc")),
                Some(mode) if mode.starts_with('-') => {
                    result.diagnostics.push(error("--mode requires text, json, or rpc"))
                }
                Some(mode) => {
                    i += 1;
                    match mode.as_str() {
                        "text" => result.mode = Some(Mode::Text),
                        "json" => result.mode = Some(Mode::Json),
                        "rpc" => result.mode = Some(Mode::Rpc),
                        _ => result
                            .diagnostics
                            .push(error(format!("Invalid mode \"{mode}\". Valid values: text, json, rpc"))),
                    }
                }
            }
        } else if arg == "--continue" || arg == "-c" {
            result.r#continue = Some(true);
        } else if arg == "--resume" || arg == "-r" {
            result.resume = Some(true);
        } else if arg == "--provider" && has_next(i) {
            i += 1;
            result.provider = Some(args[i].clone());
        } else if arg == "--model" && has_next(i) {
            i += 1;
            result.model = Some(args[i].clone());
        } else if arg == "--api-key" && has_next(i) {
            i += 1;
            result.api_key = Some(args[i].clone());
        } else if arg == "--system-prompt" && has_next(i) {
            i += 1;
            result.system_prompt = Some(args[i].clone());
        } else if arg == "--append-system-prompt" && has_next(i) {
            i += 1;
            result.append_system_prompt.get_or_insert_with(Vec::new).push(args[i].clone());
        } else if arg == "--name" || arg == "-n" {
            if has_next(i) {
                i += 1;
                result.name = Some(args[i].clone());
            } else {
                result.diagnostics.push(error("--name requires a value"));
            }
        } else if arg == "--no-session" {
            result.no_session = Some(true);
        } else if arg == "--session" && has_next(i) {
            i += 1;
            result.session = Some(args[i].clone());
        } else if arg == "--session-id" && has_next(i) {
            i += 1;
            result.session_id = Some(args[i].clone());
        } else if arg == "--fork" && has_next(i) {
            i += 1;
            result.fork = Some(args[i].clone());
        } else if arg == "--session-dir" && has_next(i) {
            i += 1;
            result.session_dir = Some(args[i].clone());
        } else if arg == "--models" && has_next(i) {
            i += 1;
            result.models = Some(split_list(&args[i]));
        } else if arg == "--no-tools" || arg == "-nt" {
            result.no_tools = Some(true);
        } else if arg == "--no-builtin-tools" || arg == "-nbt" {
            result.no_builtin_tools = Some(true);
        } else if (arg == "--tools" || arg == "-t") && has_next(i) {
            i += 1;
            let tools = split_list(&args[i]);
            match get_tool_list_error(&tools) {
                Some(message) => result.diagnostics.push(error(format!("{arg}: {message}"))),
                None => result.tools = Some(tools),
            }
        } else if (arg == "--exclude-tools" || arg == "-xt") && has_next(i) {
            i += 1;
            result.exclude_tools = Some(split_list(&args[i]));
        } else if arg == "--thinking" && has_next(i) {
            i += 1;
            let level = &args[i];
            match ThinkingLevel::parse(level).filter(|_| is_valid_thinking_level(level)) {
                Some(parsed) => result.thinking = Some(parsed),
                None => result.diagnostics.push(ArgsDiagnostics {
                    r#type: ArgsDiagnosticsType::Warning,
                    message: format!(
                        "Invalid thinking level \"{level}\". Valid values: {}",
                        VALID_THINKING_LEVELS.join(", ")
                    ),
                }),
            }
        } else if arg == "--print" || arg == "-p" {
            result.print = Some(true);
            if let Some(next) = args.get(i + 1)
                && !next.starts_with('@')
                && (!next.starts_with('-') || next.starts_with("---"))
            {
                result.messages.push(next.clone());
                i += 1;
            }
        } else if arg == "--export" && has_next(i) {
            i += 1;
            result.export = Some(args[i].clone());
        } else if (arg == "--extension" || arg == "-e") && has_next(i) {
            i += 1;
            result.extensions.get_or_insert_with(Vec::new).push(args[i].clone());
        } else if arg == "--no-extensions" || arg == "-ne" {
            result.no_extensions = Some(true);
        } else if arg == "--no-mcp" {
            result.no_mcp = Some(true);
        } else if arg == "--skill" && has_next(i) {
            i += 1;
            result.skills.get_or_insert_with(Vec::new).push(args[i].clone());
        } else if arg == "--prompt-template" && has_next(i) {
            i += 1;
            result.prompt_templates.get_or_insert_with(Vec::new).push(args[i].clone());
        } else if arg == "--theme" && has_next(i) {
            i += 1;
            result.themes.get_or_insert_with(Vec::new).push(args[i].clone());
        } else if arg == "--use-theme" {
            match args.get(i + 1) {
                Some(theme_name) if !theme_name.starts_with('-') => {
                    result.use_theme = Some(theme_name.clone());
                    i += 1;
                }
                _ => result.diagnostics.push(error("--use-theme requires a theme name")),
            }
        } else if arg == "--no-skills" || arg == "-ns" {
            result.no_skills = Some(true);
        } else if arg == "--no-prompt-templates" || arg == "-np" {
            result.no_prompt_templates = Some(true);
        } else if arg == "--no-themes" {
            result.no_themes = Some(true);
        } else if arg == "--no-context-files" || arg == "-nc" {
            result.no_context_files = Some(true);
        } else if arg == "--list-models" {
            // Check if next arg is a search pattern (not a flag or file arg)
            match args.get(i + 1) {
                Some(next) if !next.starts_with('-') && !next.starts_with('@') => {
                    result.list_models = Some(ArgsListModels::String(next.clone()));
                    i += 1;
                }
                _ => result.list_models = Some(ArgsListModels::Bool(true)),
            }
        } else if arg == "--tui-mode" {
            match args.get(i + 1).map(String::as_str) {
                Some("regular") => {
                    result.tui_mode = Some(TuiMode::Regular);
                    i += 1;
                }
                Some("fullscreen") => {
                    result.tui_mode = Some(TuiMode::Fullscreen);
                    i += 1;
                }
                None => result.diagnostics.push(error("--tui-mode requires regular or fullscreen")),
                Some(mode) if mode.starts_with('-') => {
                    result.diagnostics.push(error("--tui-mode requires regular or fullscreen"))
                }
                Some(mode) => {
                    i += 1;
                    result
                        .diagnostics
                        .push(error(format!("Invalid TUI mode \"{mode}\". Valid values: regular, fullscreen")));
                }
            }
        } else if arg == "--verbose" {
            result.verbose = Some(true);
        } else if arg == "--approve" || arg == "-a" {
            result.project_trust_override = Some(true);
        } else if arg == "--no-approve" || arg == "-na" {
            result.project_trust_override = Some(false);
        } else if arg == "--offline" {
            result.offline = Some(true);
        } else if let Some(file) = arg.strip_prefix('@') {
            result.file_args.push(file.to_string()); // Remove @ prefix
        } else if let Some(flag) = arg.strip_prefix("--") {
            if let Some(eq_index) = flag.find('=') {
                result.unknown_flags.insert(
                    flag[..eq_index].to_string(),
                    ArgsUnknownFlags::String(flag[eq_index + 1..].to_string()),
                );
            } else {
                match args.get(i + 1) {
                    Some(next) if !next.starts_with('-') && !next.starts_with('@') => {
                        result.unknown_flags.insert(flag.to_string(), ArgsUnknownFlags::String(next.clone()));
                        i += 1;
                    }
                    _ => {
                        result.unknown_flags.insert(flag.to_string(), ArgsUnknownFlags::Bool(true));
                    }
                }
            }
        } else if arg.starts_with('-') {
            result.diagnostics.push(error(format!("Unknown option: {arg}")));
        } else {
            result.messages.push(arg.to_string());
        }
        i += 1;
    }

    result
}

/// JS `String.prototype.padEnd` (by UTF-16 length).
fn pad_end(value: &str, width: usize) -> String {
    let len = value.encode_utf16().count();
    if len >= width { value.to_string() } else { format!("{value}{}", " ".repeat(width - len)) }
}

/// The help text printed by [`print_help`].
pub fn help_text(extension_flags: Option<&[ExtensionFlag]>) -> String {
    let extension_flags_text = match extension_flags {
        Some(flags) if !flags.is_empty() => format!(
            "\n{}\n{}\n",
            chalk::bold("Extension CLI Flags:"),
            flags
                .iter()
                .map(|flag| {
                    let value = if flag.flag_type == FlagType::String { " <value>" } else { "" };
                    let description = flag
                        .description
                        .clone()
                        .unwrap_or_else(|| format!("Registered by {}", flag.extension_path));
                    format!("{}{description}", pad_end(&format!("  --{}{value}", flag.name), 30))
                })
                .collect::<Vec<_>>()
                .join("\n")
        ),
        _ => String::new(),
    };
    let bold_app = chalk::bold(APP_NAME);
    let usage = chalk::bold("Usage:");
    let commands = chalk::bold("Commands:");
    let options = chalk::bold("Options:");
    let examples = chalk::bold("Examples:");
    let environment = chalk::bold("Environment Variables:");
    let builtin_tools = chalk::bold("Built-in Tool Names:");
    let a = APP_NAME;
    let c = CONFIG_DIR_NAME;
    let env_agent_dir = pad_end(ENV_AGENT_DIR, 32);
    let env_session_dir = pad_end(ENV_SESSION_DIR, 32);
    format!(
        r#"{bold_app} - AI coding assistant with read, bash, edit, write tools

{usage}
  {a} [options] [--] [@files...] [messages...]

{commands}
  {a} install <source> [-l]     Install extension source and add to settings
  {a} remove <source> [-l]      Remove extension source from settings
  {a} uninstall <source> [-l]   Alias for remove
  {a} update [source|self|pi]   Update pi, extensions, or model catalogs
  {a} list                      List installed extensions from settings
  {a} config [-l]               Open TUI to enable/disable package resources (Tab switches scope)
  {a} auth <command>            Print credentials or check provider readiness
  {a} mcp <command>             Check MCP servers, sign in to or out of OAuth servers
  {a} <command> --help          Show help for install/remove/uninstall/update/list/config/auth/mcp

{options}
  --provider <name>              Provider to search for --model (requires --model)
  --model <pattern>              Model pattern or ID (supports "provider/id" and optional ":<thinking>")
  --api-key <key>                API key (defaults to env vars)
  --system-prompt <text>         System prompt (default: coding assistant prompt)
  --append-system-prompt <text>  Append text or file contents to the system prompt (can be used multiple times)
  --mode <mode>                  Output mode: text (default), json, or rpc
  --print, -p                    Non-interactive mode: process prompt and exit
  --continue, -c                 Continue previous session
  --resume, -r                   Select a session to resume
  --session <path|id>            Use specific session file or partial UUID
  --session-id <id>              Use exact project session ID, creating it if missing
  --fork <path|id>               Fork specific session file or partial UUID into a new session
  --session-dir <dir>            Directory for session storage and lookup
  --no-session                   Don't save session (ephemeral)
  --name, -n <name>              Set session display name
  --models <patterns>            Comma-separated model patterns for Ctrl+P cycling
                                 Supports globs (anthropic/*, *sonnet*) and fuzzy matching
  --no-tools, -nt                Disable all tools by default (built-in and extension)
  --no-builtin-tools, -nbt       Disable built-in tools by default but keep extension/custom tools enabled
  --tools, -t <tools>            Comma-separated allowlist of tool names or patterns (*) to enable
                                 Keeps MCP tools unless an entry starts with mcp__
                                 Only +name/-name entries add to or remove from the defaults
  --exclude-tools, -xt <tools>   Comma-separated denylist of tool names or patterns (*) to disable
                                 Applies to all tools, MCP tools included
  --thinking <level>             Set thinking level: off, minimal, low, medium, high, xhigh, max
  --extension, -e <path>         Load an extension file or builtin:<name> (can be used multiple times)
  --no-extensions, -ne           Disable extension discovery and built-in extensions (explicit -e and --tools requests still work)
  --no-mcp                       Disable built-in MCP support: no servers connect and no MCP tools
  --skill <path>                 Load a skill file or directory (can be used multiple times)
  --no-skills, -ns               Disable skills discovery and loading
  --prompt-template <path>       Load a prompt template file or directory (can be used multiple times)
  --no-prompt-templates, -np     Disable prompt template discovery and loading
  --theme <path>                 Load a theme file or directory (can be used multiple times)
  --use-theme <name[/name]>      Set the initial interactive theme for this run
  --no-themes                    Disable theme discovery and loading
  --no-context-files, -nc        Disable AGENTS.md and CLAUDE.md discovery and loading
  --export <file>                Export session file to HTML and exit
  --list-models [search]         List available models (with optional fuzzy search)
  --verbose                      Force verbose startup (overrides quietStartup setting)
  --tui-mode <mode>              TUI mode: fullscreen (default) or regular
  --approve, -a                  Trust project-local files for this run
  --no-approve, -na              Ignore project-local files for this run
  --offline                      Disable startup network operations (same as PI_OFFLINE=1)
  --                             End option parsing; treat remaining arguments as messages/files
  --help, -h                     Show this help
  --version, -v                  Show version number

Extensions can register additional flags (e.g., --plan from plan-mode extension).{extension_flags_text}

{examples}
  # Print a provider API key for an external client
  {a} auth print-api-key --provider openai

  # Print an OAuth bearer token for an external client (refreshes if expired)
  {a} auth print-bearer-token --provider openai-codex

  # Interactive mode
  {a}

  # Interactive mode with initial prompt
  {a} "List all .ts files in src/"

  # Include files in initial message
  {a} @prompt.md @image.png "What color is the sky?"

  # Non-interactive mode (process and exit)
  {a} -p "List all .ts files in src/"

  # Prompt beginning with a dash
  {a} -p -- "- Summarize these points"

  # Multiple messages (interactive)
  {a} "Read package.json" "What dependencies do we have?"

  # Continue previous session
  {a} --continue "What did we discuss?"

  # Start a named session
  {a} --name "Refactor auth module"

  # Use different model
  {a} --provider openai --model gpt-4o-mini "Help me refactor this code"

  # Use model with provider prefix (no --provider needed)
  {a} --model openai/gpt-4o "Help me refactor this code"

  # Use model with thinking level shorthand
  {a} --model sonnet:high "Solve this complex problem"

  # Limit model cycling to specific models
  {a} --models claude-sonnet,claude-haiku,gpt-4o

  # Limit to a specific provider with glob pattern
  {a} --models "github-copilot/*"

  # Cycle models with fixed thinking levels
  {a} --models sonnet:high,haiku:low

  # Start with a specific thinking level
  {a} --thinking high "Solve this complex problem"

  # Read-only mode (no file modifications possible)
  {a} --tools read,grep,find,ls -p "Review the code in src/"

  # Add codemode to the default tools
  {a} --tools +codemode

  # Codemode with only the tools of one MCP server
  {a} --tools read,bash,codemode,'mcp__radius__*'

  # Disable one tool while keeping the rest available
  {a} --exclude-tools ask_question

  # Export a session file to HTML
  {a} --export ~/{c}/agent/sessions/--path--/session.jsonl
  {a} --export session.jsonl output.html

{environment}
  ANTHROPIC_AUTH_TOKEN             - Anthropic bearer auth token
  ANTHROPIC_API_KEY                - Anthropic Claude API key
  ANTHROPIC_OAUTH_TOKEN            - Anthropic OAuth token (alternative to API key)
  ANT_LING_API_KEY                 - Ant Ling API key
  OPENAI_API_KEY                   - OpenAI GPT API key
  AZURE_OPENAI_API_KEY             - Azure OpenAI API key
  AZURE_OPENAI_BASE_URL            - Azure OpenAI/Cognitive Services base URL (e.g. https://{{resource}}.openai.azure.com)
  AZURE_OPENAI_RESOURCE_NAME       - Azure OpenAI resource name (alternative to base URL)
  AZURE_OPENAI_API_VERSION         - Azure OpenAI API version (default: v1)
  AZURE_OPENAI_DEPLOYMENT_NAME_MAP - Azure OpenAI model=deployment map (comma-separated)
  DEEPSEEK_API_KEY                 - DeepSeek API key
  NVIDIA_API_KEY                   - NVIDIA NIM API key
  GEMINI_API_KEY                   - Google Gemini API key
  GROQ_API_KEY                     - Groq API key
  CEREBRAS_API_KEY                 - Cerebras API key
  XAI_API_KEY                      - xAI Grok API key
  FIREWORKS_API_KEY                - Fireworks API key
  TOGETHER_API_KEY                 - Together AI API key
  BASETEN_API_KEY                  - Baseten API key
  OPENROUTER_API_KEY               - OpenRouter API key
  AI_GATEWAY_API_KEY               - Vercel AI Gateway API key
  ZAI_API_KEY                      - ZAI Coding Plan API key (Global)
  ZAI_CODING_CN_API_KEY            - ZAI Coding Plan API key (China)
  MISTRAL_API_KEY                  - Mistral API key
  MINIMAX_API_KEY                  - MiniMax API key
  MOONSHOT_API_KEY                 - Moonshot AI API key
  OPENCODE_API_KEY                 - OpenCode Zen/OpenCode Go API key
  KIMI_API_KEY                     - Kimi For Coding API key
  META_API_KEY                     - Meta Model API key
  CLOUDFLARE_API_KEY               - Cloudflare API token (Workers AI and AI Gateway)
  CLOUDFLARE_ACCOUNT_ID            - Cloudflare account id (required for both)
  CLOUDFLARE_GATEWAY_ID            - Cloudflare AI Gateway slug (required for AI Gateway)
  QWEN_TOKEN_PLAN_API_KEY          - Qwen Token Plan API key (international region)
  QWEN_TOKEN_PLAN_CN_API_KEY       - Qwen Token Plan API key (China region)
  XIAOMI_API_KEY                   - Xiaomi MiMo API key (api.xiaomimimo.com billing)
  XIAOMI_TOKEN_PLAN_CN_API_KEY     - Xiaomi MiMo Token Plan API key (China region)
  XIAOMI_TOKEN_PLAN_AMS_API_KEY    - Xiaomi MiMo Token Plan API key (Amsterdam region)
  XIAOMI_TOKEN_PLAN_SGP_API_KEY    - Xiaomi MiMo Token Plan API key (Singapore region)
  AWS_PROFILE                      - AWS profile for Amazon Bedrock
  AWS_ACCESS_KEY_ID                - AWS access key for Amazon Bedrock
  AWS_SECRET_ACCESS_KEY            - AWS secret key for Amazon Bedrock
  AWS_BEARER_TOKEN_BEDROCK         - Bedrock API key (bearer token)
  AWS_REGION                       - AWS region for Amazon Bedrock (e.g., us-east-1)
  {env_agent_dir} - Config directory (default: ~/{c}/agent)
  {env_session_dir} - Session storage directory (overridden by --session-dir)
  PI_PACKAGE_DIR                   - Override package directory (for Nix/Guix store paths)
  PI_OFFLINE                       - Disable startup network operations when set to 1/true/yes
  PI_TELEMETRY                     - Override install telemetry when set to 1/true/yes or 0/false/no
  PI_SHARE_VIEWER_URL              - Base URL for /share command (default: https://pi.dev/session/)

{builtin_tools}
  read       - Read file contents
  bash       - Execute bash commands
  powershell - Execute PowerShell commands on Windows
  edit       - Edit files with find/replace
  write      - Write files (creates/overwrites)
  grep       - Search file contents (read-only, off by default)
  find       - Find files by glob pattern (read-only, off by default)
  ls         - List directory contents (read-only, off by default)
"#
    )
}

pub fn print_help(extension_flags: Option<&[ExtensionFlag]>) {
    println!("{}", help_text(extension_flags));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn err(message: &str) -> Vec<ArgsDiagnostics> {
        vec![ArgsDiagnostics { r#type: ArgsDiagnosticsType::Error, message: message.to_string() }]
    }

    #[test]
    fn version_flag() {
        assert_eq!(parse(&["--version"]).version, Some(true));
        assert_eq!(parse(&["-v"]).version, Some(true));
        let result = parse(&["--version", "--help", "some message"]);
        assert_eq!(result.version, Some(true));
        assert_eq!(result.help, Some(true));
        assert!(result.messages.contains(&"some message".to_string()));
    }

    #[test]
    fn help_flag() {
        assert_eq!(parse(&["--help"]).help, Some(true));
        assert_eq!(parse(&["-h"]).help, Some(true));
    }

    #[test]
    fn print_flag() {
        assert_eq!(parse(&["--print"]).print, Some(true));
        assert_eq!(parse(&["-p"]).print, Some(true));
        let prompt = "---\ntitle: hello\n---\nSay hi.";
        let result = parse(&["-p", prompt]);
        assert_eq!(result.print, Some(true));
        assert_eq!(result.messages, strings(&[prompt]));
        assert!(result.unknown_flags.is_empty());
        let result = parse(&["-p", "--provider", "openai", "Say hi."]);
        assert_eq!(result.print, Some(true));
        assert_eq!(result.provider.as_deref(), Some("openai"));
        assert_eq!(result.messages, strings(&["Say hi."]));
    }

    #[test]
    fn continue_and_resume_flags() {
        assert_eq!(parse(&["--continue"]).r#continue, Some(true));
        assert_eq!(parse(&["-c"]).r#continue, Some(true));
        assert_eq!(parse(&["--resume"]).resume, Some(true));
        assert_eq!(parse(&["-r"]).resume, Some(true));
    }

    #[test]
    fn flags_with_values() {
        assert_eq!(parse(&["--provider", "openai"]).provider.as_deref(), Some("openai"));
        assert_eq!(parse(&["--model", "gpt-4o"]).model.as_deref(), Some("gpt-4o"));
        assert_eq!(parse(&["--api-key", "sk-test-key"]).api_key.as_deref(), Some("sk-test-key"));
        assert_eq!(
            parse(&["--system-prompt", "You are a helpful assistant"]).system_prompt.as_deref(),
            Some("You are a helpful assistant")
        );
        assert_eq!(
            parse(&["--append-system-prompt", "Additional context"]).append_system_prompt,
            Some(strings(&["Additional context"]))
        );
        assert_eq!(
            parse(&["--append-system-prompt", "Context A", "--append-system-prompt", "Context B"]).append_system_prompt,
            Some(strings(&["Context A", "Context B"]))
        );
        assert_eq!(parse(&["--session", "/path/to/session.jsonl"]).session.as_deref(), Some("/path/to/session.jsonl"));
        assert_eq!(
            parse(&["--session-id", "orchestrated-session"]).session_id.as_deref(),
            Some("orchestrated-session")
        );
        let result = parse(&["--fork", "1234abcd"]);
        assert_eq!(result.fork.as_deref(), Some("1234abcd"));
        assert!(result.messages.is_empty());
        assert_eq!(parse(&["--export", "session.jsonl"]).export.as_deref(), Some("session.jsonl"));
        assert_eq!(parse(&["--thinking", "high"]).thinking, Some(ThinkingLevel::High));
        assert_eq!(
            parse(&["--models", "gpt-4o,claude-sonnet,gemini-pro"]).models,
            Some(strings(&["gpt-4o", "claude-sonnet", "gemini-pro"]))
        );
        // Issue #10334
        assert_eq!(parse(&["--models", "gpt-4o, ,claude-sonnet,"]).models, Some(strings(&["gpt-4o", "claude-sonnet"])));
    }

    #[test]
    fn invalid_thinking_level_warns() {
        let result = parse(&["--thinking", "extreme"]);
        assert_eq!(result.thinking, None);
        assert_eq!(
            result.diagnostics,
            vec![ArgsDiagnostics {
                r#type: ArgsDiagnosticsType::Warning,
                message: "Invalid thinking level \"extreme\". Valid values: off, minimal, low, medium, high, xhigh, max"
                    .into()
            }]
        );
    }

    // Issue #9045
    #[test]
    fn mode_flag() {
        for (mode, expected) in [("text", Mode::Text), ("json", Mode::Json), ("rpc", Mode::Rpc)] {
            let result = parse(&["--mode", mode]);
            assert_eq!(result.mode, Some(expected));
            assert!(result.diagnostics.is_empty());
        }
        for mode in ["yaml", ""] {
            let result = parse(&["--mode", mode, "--version"]);
            assert_eq!(result.mode, None);
            assert_eq!(result.version, Some(true));
            assert!(result.messages.is_empty());
            assert!(result.unknown_flags.is_empty());
            assert_eq!(result.diagnostics, err(&format!("Invalid mode \"{mode}\". Valid values: text, json, rpc")));
        }
        let result = parse(&["--mode"]);
        assert_eq!(result.mode, None);
        assert!(result.unknown_flags.is_empty());
        assert_eq!(result.diagnostics, err("--mode requires text, json, or rpc"));
        let result = parse(&["--mode", "--version"]);
        assert_eq!(result.mode, None);
        assert_eq!(result.version, Some(true));
        assert!(result.unknown_flags.is_empty());
        assert_eq!(result.diagnostics, err("--mode requires text, json, or rpc"));
        let result = parse(&["--mode", "json", "--mode", "yaml"]);
        assert_eq!(result.diagnostics, err("Invalid mode \"yaml\". Valid values: text, json, rpc"));
    }

    #[test]
    fn name_flag() {
        assert_eq!(parse(&["--name", "my-session"]).name.as_deref(), Some("my-session"));
        assert_eq!(parse(&["-n", "quick-session"]).name.as_deref(), Some("quick-session"));
        assert_eq!(parse(&["--name", ""]).name.as_deref(), Some(""));
        assert_eq!(normalize_session_name("  named session  ").as_deref(), Some("named session"));
        assert_eq!(normalize_session_name("   "), None);
        assert_eq!(parse(&["--name"]).diagnostics, err("--name requires a value"));
        let result = parse(&["--name", "named-run", "--print", "--model", "gpt-4o", "hello"]);
        assert_eq!(result.name.as_deref(), Some("named-run"));
        assert_eq!(result.print, Some(true));
        assert_eq!(result.model.as_deref(), Some("gpt-4o"));
        assert_eq!(result.messages, strings(&["hello"]));
    }

    #[test]
    fn no_session_flag() {
        assert_eq!(parse(&["--no-session"]).no_session, Some(true));
        let result = parse(&["--session-id", "ephemeral-id", "--help"]);
        assert_eq!((result.session_id.as_deref(), result.help), (Some("ephemeral-id"), Some(true)));
        let result = parse(&["--session-id", "ephemeral-id", "--list-models"]);
        assert_eq!(result.session_id.as_deref(), Some("ephemeral-id"));
        assert_eq!(result.list_models, Some(ArgsListModels::Bool(true)));
        let result = parse(&["--session-id", "ephemeral-id", "--no-session"]);
        assert_eq!((result.session_id.as_deref(), result.no_session), (Some("ephemeral-id"), Some(true)));
    }

    #[test]
    fn resource_flags() {
        assert_eq!(parse(&["--extension", "./my-extension.ts"]).extensions, Some(strings(&["./my-extension.ts"])));
        assert_eq!(parse(&["-e", "./my-extension.ts"]).extensions, Some(strings(&["./my-extension.ts"])));
        assert_eq!(
            parse(&["--extension", "./ext1.ts", "-e", "./ext2.ts"]).extensions,
            Some(strings(&["./ext1.ts", "./ext2.ts"]))
        );
        assert_eq!(parse(&["--no-extensions"]).no_extensions, Some(true));
        let result = parse(&["--no-extensions", "-e", "foo.ts", "-e", "bar.ts"]);
        assert_eq!(result.no_extensions, Some(true));
        assert_eq!(result.extensions, Some(strings(&["foo.ts", "bar.ts"])));
        let result = parse(&["--no-mcp"]);
        assert_eq!(result.no_mcp, Some(true));
        assert!(result.unknown_flags.is_empty());
        assert_eq!(parse(&["--skill", "./skill-dir"]).skills, Some(strings(&["./skill-dir"])));
        assert_eq!(
            parse(&["--skill", "./skill-a", "--skill", "./skill-b"]).skills,
            Some(strings(&["./skill-a", "./skill-b"]))
        );
        assert_eq!(parse(&["--prompt-template", "./prompts"]).prompt_templates, Some(strings(&["./prompts"])));
        assert_eq!(
            parse(&["--prompt-template", "./one", "--prompt-template", "./two"]).prompt_templates,
            Some(strings(&["./one", "./two"]))
        );
        assert_eq!(parse(&["--theme", "./theme.json"]).themes, Some(strings(&["./theme.json"])));
        assert_eq!(
            parse(&["--theme", "./dark.json", "--theme", "./light.json"]).themes,
            Some(strings(&["./dark.json", "./light.json"]))
        );
        assert_eq!(parse(&["--use-theme", "light"]).use_theme.as_deref(), Some("light"));
        let result = parse(&["--use-theme", "--print"]);
        assert_eq!(result.use_theme, None);
        assert_eq!(result.print, Some(true));
        assert_eq!(result.diagnostics, err("--use-theme requires a theme name"));
        assert_eq!(parse(&["--no-skills"]).no_skills, Some(true));
        assert_eq!(parse(&["--no-prompt-templates"]).no_prompt_templates, Some(true));
        assert_eq!(parse(&["--no-themes"]).no_themes, Some(true));
        assert_eq!(parse(&["--no-context-files"]).no_context_files, Some(true));
        assert_eq!(parse(&["-nc"]).no_context_files, Some(true));
    }

    #[test]
    fn project_approval_and_misc_flags() {
        assert_eq!(parse(&["--approve"]).project_trust_override, Some(true));
        assert_eq!(parse(&["-a"]).project_trust_override, Some(true));
        assert_eq!(parse(&["--no-approve"]).project_trust_override, Some(false));
        assert_eq!(parse(&["-na"]).project_trust_override, Some(false));
        assert_eq!(parse(&["--verbose"]).verbose, Some(true));
        assert_eq!(parse(&["--offline"]).offline, Some(true));
    }

    #[test]
    fn tui_mode_flag() {
        assert_eq!(parse(&["--tui-mode", "regular"]).tui_mode, Some(TuiMode::Regular));
        assert_eq!(parse(&["--tui-mode", "fullscreen"]).tui_mode, Some(TuiMode::Fullscreen));
        assert_eq!(
            parse(&["--tui-mode", "other"]).diagnostics,
            err("Invalid TUI mode \"other\". Valid values: regular, fullscreen")
        );
        assert_eq!(parse(&["--tui-mode"]).diagnostics, err("--tui-mode requires regular or fullscreen"));
        let result = parse(&["--ui-mode", "fullscreen"]);
        assert_eq!(result.tui_mode, None);
        assert_eq!(result.unknown_flags.get("ui-mode"), Some(&ArgsUnknownFlags::String("fullscreen".into())));
    }

    #[test]
    fn tool_flags() {
        assert_eq!(parse(&["--no-tools"]).no_tools, Some(true));
        assert_eq!(parse(&["-nt"]).no_tools, Some(true));
        assert_eq!(parse(&["--no-builtin-tools"]).no_builtin_tools, Some(true));
        assert_eq!(parse(&["-nbt"]).no_builtin_tools, Some(true));
        assert_eq!(parse(&["--tools", "read,bash"]).tools, Some(strings(&["read", "bash"])));
        assert_eq!(parse(&["-t", "read,bash"]).tools, Some(strings(&["read", "bash"])));
        let result = parse(&["-t", "+codemode,-write"]);
        assert_eq!(result.tools, Some(strings(&["+codemode", "-write"])));
        assert!(result.diagnostics.is_empty());
        let result = parse(&["--tools", "read,+codemode"]);
        assert_eq!(result.tools, None);
        assert_eq!(result.diagnostics, err("--tools: tool names cannot be mixed with +name or -name entries"));
        let result = parse(&["-t", "+mcp__radius__*"]);
        assert_eq!(result.tools, None);
        assert_eq!(
            result.diagnostics,
            err("-t: +name and -name entries take exact tool names, not patterns: +mcp__radius__*")
        );
        assert_eq!(parse(&["--exclude-tools", "read,bash"]).exclude_tools, Some(strings(&["read", "bash"])));
        assert_eq!(parse(&["-xt", "read,bash"]).exclude_tools, Some(strings(&["read", "bash"])));
        let result = parse(&["--no-tools", "--tools", "read,bash"]);
        assert_eq!(result.no_tools, Some(true));
        assert_eq!(result.tools, Some(strings(&["read", "bash"])));
        let result = parse(&["--no-builtin-tools", "--tools", "read,bash"]);
        assert_eq!(result.no_builtin_tools, Some(true));
        assert_eq!(result.tools, Some(strings(&["read", "bash"])));
    }

    #[test]
    fn messages_and_file_args() {
        assert_eq!(parse(&["hello", "world"]).messages, strings(&["hello", "world"]));
        assert_eq!(parse(&["@README.md", "@src/main.ts"]).file_args, strings(&["README.md", "src/main.ts"]));
        let result = parse(&["@file.txt", "explain this", "@image.png"]);
        assert_eq!(result.file_args, strings(&["file.txt", "image.png"]));
        assert_eq!(result.messages, strings(&["explain this"]));
        let result = parse(&["--unknown-flag", "message"]);
        assert!(result.messages.is_empty());
        assert_eq!(result.unknown_flags.get("unknown-flag"), Some(&ArgsUnknownFlags::String("message".into())));
        assert_eq!(parse(&["--unknown-flag"]).unknown_flags.get("unknown-flag"), Some(&ArgsUnknownFlags::Bool(true)));
        assert_eq!(
            parse(&["--unknown-flag=value"]).unknown_flags.get("unknown-flag"),
            Some(&ArgsUnknownFlags::String("value".into()))
        );
        assert_eq!(parse(&["-x"]).diagnostics, err("Unknown option: -x"));
    }

    // suite/regressions/7269-cli-end-of-options
    #[test]
    fn end_of_options() {
        for prompt in ["- summarize the following points for me", "--answer my question briefly"] {
            let result = parse(&["-ne", "--no-session", "-p", "--", prompt]);
            assert_eq!(result.messages, strings(&[prompt]));
            assert!(result.unknown_flags.is_empty());
            assert!(result.diagnostics.is_empty());
        }
        let result = parse(&["--unknown-flag", "value", "--", "--provider", "openai", "-c", "@prompt.md"]);
        assert_eq!(result.unknown_flags.get("unknown-flag"), Some(&ArgsUnknownFlags::String("value".into())));
        assert_eq!(result.provider, None);
        assert_eq!(result.r#continue, None);
        assert_eq!(result.messages, strings(&["--provider", "openai", "-c"]));
        assert_eq!(result.file_args, strings(&["prompt.md"]));
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn complex_combinations() {
        let result = parse(&[
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet",
            "--print",
            "--thinking",
            "high",
            "@prompt.md",
            "Do the task",
        ]);
        assert_eq!(result.provider.as_deref(), Some("anthropic"));
        assert_eq!(result.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(result.print, Some(true));
        assert_eq!(result.thinking, Some(ThinkingLevel::High));
        assert_eq!(result.file_args, strings(&["prompt.md"]));
        assert_eq!(result.messages, strings(&["Do the task"]));
    }

    #[test]
    fn help_lists_extension_flags() {
        let flags = vec![
            ExtensionFlag {
                name: "plan".into(),
                description: Some("Start in plan mode".into()),
                flag_type: FlagType::Boolean,
                default: None,
                extension_path: "/x/plan.ts".into(),
            },
            ExtensionFlag {
                name: "level".into(),
                description: None,
                flag_type: FlagType::String,
                default: None,
                extension_path: "/x/level.ts".into(),
            },
        ];
        let text = help_text(Some(&flags));
        assert!(text.contains("\n  --plan                      Start in plan mode\n"));
        assert!(text.contains("\n  --level <value>             Registered by /x/level.ts\n"));
        assert!(text.contains("  PI_CODING_AGENT_DIR              - Config directory (default: ~/.pi/agent)"));
        assert!(text.ends_with("  ls         - List directory contents (read-only, off by default)\n"));
    }
}
