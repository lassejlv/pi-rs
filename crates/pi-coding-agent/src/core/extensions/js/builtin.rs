//! Built-in tools and coding-agent helpers exposed to JavaScript extensions
//! (`createReadTool()`, `truncateHead()`, `getAgentDir()`, ...).

use std::sync::Arc;

use pi_jsrt::JsHandle;
use serde_json::{Value, json};

use super::api::{arg, js_bash_operations, opt, str_arg};
use super::provider::bytes_of;
use super::{Bridge, CtxEntry, component};
use crate::core::extensions::types::*;
use crate::core::tools::bash::{BashSpawnContext, CreateLocalBashOperationsOptions, create_local_bash_operations};
use crate::core::tools::edit::EditOperations;
use crate::core::tools::find::FindOperations;
use crate::core::tools::grep::GrepOperations;
use crate::core::tools::ls::{LsOperations, LsOperationsResult};
use crate::core::tools::read::ReadOperations;
use crate::core::tools::write::WriteOperations;

/// A built-in tool created by an extension (`createXToolDefinition()` / `createXTool()`).
#[derive(Clone)]
pub(crate) struct BuiltinTool {
    pub(crate) definition: ToolDefinition,
}

/// An async JS function `(...args) => Promise<T>` as a Rust closure returning JSON.
fn js_async_fn(bridge: &Arc<Bridge>, handle: JsHandle) -> impl Fn(Vec<Value>) -> futures::future::BoxFuture<'static, anyhow::Result<Value>> + Send + Sync + Clone + 'static {
    let bridge = bridge.clone();
    move |args: Vec<Value>| {
        let bridge = bridge.clone();
        let handle = handle.clone();
        Box::pin(async move { bridge.glue_call("callFunctionAsync", vec![handle.to_value(), Value::Array(args)]).await })
    }
}

fn read_operations(bridge: &Arc<Bridge>, ops: &Value) -> Option<ReadOperations> {
    let read_file = js_async_fn(bridge, bridge.handle(&ops["readFile"])?);
    let access = bridge.handle(&ops["access"]).map(|h| js_async_fn(bridge, h));
    let detect = bridge.handle(&ops["detectImageMimeType"]).map(|h| js_async_fn(bridge, h));
    Some(ReadOperations {
        read_file: Arc::new(move |path| {
            let f = read_file.clone();
            Box::pin(async move { Ok(bytes_of(&f(vec![json!(path)]).await?)) })
        }),
        access: match access {
            Some(f) => Arc::new(move |path| {
                let f = f.clone();
                Box::pin(async move { f(vec![json!(path)]).await.map(|_| ()) })
            }),
            None => Arc::new(|path| Box::pin(async move { tokio::fs::metadata(&path).await.map(|_| ()).map_err(Into::into) })),
        },
        detect_image_mime_type: detect.map(|f| {
            Arc::new(move |path: String| {
                let f = f.clone();
                Box::pin(async move { Ok(f(vec![json!(path)]).await?.as_str().map(str::to_string)) })
                    as futures::future::BoxFuture<'static, anyhow::Result<Option<String>>>
            }) as Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, anyhow::Result<Option<String>>> + Send + Sync>
        }),
    })
}

fn write_operations(bridge: &Arc<Bridge>, ops: &Value) -> Option<WriteOperations> {
    let write_file = js_async_fn(bridge, bridge.handle(&ops["writeFile"])?);
    let mkdir = bridge.handle(&ops["mkdir"]).map(|h| js_async_fn(bridge, h));
    Some(WriteOperations {
        write_file: Arc::new(move |path, content| {
            let f = write_file.clone();
            Box::pin(async move { f(vec![json!(path), json!(content)]).await.map(|_| ()) })
        }),
        mkdir: match mkdir {
            Some(f) => Arc::new(move |dir| {
                let f = f.clone();
                Box::pin(async move { f(vec![json!(dir)]).await.map(|_| ()) })
            }),
            None => Arc::new(|dir| Box::pin(async move { tokio::fs::create_dir_all(&dir).await.map_err(Into::into) })),
        },
    })
}

fn edit_operations(bridge: &Arc<Bridge>, ops: &Value) -> Option<EditOperations> {
    let read_file = js_async_fn(bridge, bridge.handle(&ops["readFile"])?);
    let write_file = js_async_fn(bridge, bridge.handle(&ops["writeFile"])?);
    let access = bridge.handle(&ops["access"]).map(|h| js_async_fn(bridge, h));
    Some(EditOperations {
        read_file: Arc::new(move |path| {
            let f = read_file.clone();
            Box::pin(async move { Ok(bytes_of(&f(vec![json!(path)]).await?)) })
        }),
        write_file: Arc::new(move |path, content| {
            let f = write_file.clone();
            Box::pin(async move { f(vec![json!(path), json!(content)]).await.map(|_| ()) })
        }),
        access: match access {
            Some(f) => Arc::new(move |path| {
                let f = f.clone();
                Box::pin(async move { f(vec![json!(path)]).await.map(|_| ()) })
            }),
            None => Arc::new(|path| Box::pin(async move { tokio::fs::metadata(&path).await.map(|_| ()).map_err(Into::into) })),
        },
    })
}

fn grep_operations(bridge: &Arc<Bridge>, ops: &Value) -> Option<GrepOperations> {
    let is_directory = js_async_fn(bridge, bridge.handle(&ops["isDirectory"])?);
    let read_file = js_async_fn(bridge, bridge.handle(&ops["readFile"])?);
    Some(GrepOperations {
        is_directory: Arc::new(move |path| {
            let f = is_directory.clone();
            Box::pin(async move { Ok(f(vec![json!(path)]).await?.as_bool().unwrap_or(false)) })
        }),
        read_file: Arc::new(move |path| {
            let f = read_file.clone();
            Box::pin(async move { Ok(f(vec![json!(path)]).await?.as_str().unwrap_or_default().to_string()) })
        }),
    })
}

fn find_operations(bridge: &Arc<Bridge>, ops: &Value) -> Option<FindOperations> {
    let exists = js_async_fn(bridge, bridge.handle(&ops["exists"])?);
    let glob = js_async_fn(bridge, bridge.handle(&ops["glob"])?);
    Some(FindOperations {
        exists: Arc::new(move |path| {
            let f = exists.clone();
            Box::pin(async move { Ok(f(vec![json!(path)]).await?.as_bool().unwrap_or(false)) })
        }),
        glob: Arc::new(move |pattern, cwd, options| {
            let f = glob.clone();
            Box::pin(async move {
                let value = f(vec![json!(pattern), json!(cwd), json!({ "ignore": options.ignore, "limit": options.limit })]).await?;
                Ok(serde_json::from_value(value).unwrap_or_default())
            })
        }),
    })
}

fn ls_operations(bridge: &Arc<Bridge>, ops: &Value) -> Option<LsOperations> {
    let exists = js_async_fn(bridge, bridge.handle(&ops["exists"])?);
    let stat = js_async_fn(bridge, bridge.handle(&ops["stat"])?);
    let readdir = js_async_fn(bridge, bridge.handle(&ops["readdir"])?);
    let b = bridge.clone();
    Some(LsOperations {
        exists: Arc::new(move |path| {
            let f = exists.clone();
            Box::pin(async move { Ok(f(vec![json!(path)]).await?.as_bool().unwrap_or(false)) })
        }),
        stat: Arc::new(move |path| {
            let f = stat.clone();
            let b = b.clone();
            Box::pin(async move {
                let value = b.glue_call("statIsDirectory", vec![f(vec![json!(path)]).await?]).await?;
                let is_dir = value.as_bool().unwrap_or(false);
                Ok(LsOperationsResult { is_directory: Arc::new(move || is_dir) })
            })
        }),
        readdir: Arc::new(move |path| {
            let f = readdir.clone();
            Box::pin(async move { Ok(serde_json::from_value(f(vec![json!(path)]).await?).unwrap_or_default()) })
        }),
    })
}

fn spawn_hook(bridge: &Arc<Bridge>, value: &Value) -> Option<crate::core::tools::bash::BashSpawnHook> {
    let handle = bridge.handle(value)?;
    Some(Arc::new(move |context: BashSpawnContext| {
        let input = serde_json::to_value(&context).unwrap_or(Value::Null);
        match handle.call_blocking(vec![input]) {
            Ok(v) => serde_json::from_value(v).unwrap_or(context),
            Err(_) => context,
        }
    }))
}

fn create_definition(bridge: &Arc<Bridge>, kind: &str, cwd: &str, options: &Value) -> anyhow::Result<ToolDefinition> {
    use crate::core::tools::bash::BashToolOptions;
    use crate::core::tools::edit::EditToolOptions;
    use crate::core::tools::find::FindToolOptions;
    use crate::core::tools::grep::GrepToolOptions;
    use crate::core::tools::ls::LsToolOptions;
    use crate::core::tools::read::ReadToolOptions;
    use crate::core::tools::write::WriteToolOptions;
    use crate::core::tools::{bash, edit, find, grep, ls, powershell, read, write};
    let ops = &options["operations"];
    Ok(match kind {
        "read" => {
            let o = ReadToolOptions {
                auto_resize_images: options["autoResizeImages"].as_bool(),
                resize_options: None,
                operations: read_operations(bridge, ops),
            };
            read::create_read_tool_definition(cwd, Some(&o))
        }
        "bash" => {
            let o = BashToolOptions {
                operations: bridge.handle(&ops["exec"]).map(|h| js_bash_operations(bridge.clone(), h)),
                command_prefix: options["commandPrefix"].as_str().map(str::to_string),
                shell_path: options["shellPath"].as_str().map(str::to_string),
                expose_session_environment: options["exposeSessionEnvironment"].as_bool(),
                spawn_hook: spawn_hook(bridge, &options["spawnHook"]),
            };
            bash::create_bash_tool_definition(cwd, Some(&o))
        }
        "powershell" => {
            let o = powershell::PowerShellToolOptions {
                operations: bridge.handle(&ops["exec"]).map(|h| js_bash_operations(bridge.clone(), h)),
                expose_session_environment: options["exposeSessionEnvironment"].as_bool(),
                spawn_hook: spawn_hook(bridge, &options["spawnHook"]),
            };
            powershell::create_power_shell_tool_definition(cwd, Some(&o))
        }
        "edit" => edit::create_edit_tool_definition(cwd, Some(&EditToolOptions { operations: edit_operations(bridge, ops) })),
        "write" => write::create_write_tool_definition(cwd, Some(&WriteToolOptions { operations: write_operations(bridge, ops) })),
        "grep" => grep::create_grep_tool_definition(cwd, Some(&GrepToolOptions { operations: grep_operations(bridge, ops) })),
        "find" => find::create_find_tool_definition(cwd, Some(&FindToolOptions { operations: find_operations(bridge, ops) })),
        "ls" => ls::create_ls_tool_definition(cwd, Some(&LsToolOptions { operations: ls_operations(bridge, ops) })),
        _ => anyhow::bail!("unknown built-in tool {kind}"),
    })
}

/// `execute` of a built-in tool named by a `$piRust` marker.
pub(crate) fn rust_execute(bridge: &Arc<Bridge>, marker: &Value) -> anyhow::Result<ToolExecuteFn> {
    let id = marker["tool"].as_u64().ok_or_else(|| anyhow::anyhow!("invalid built-in tool marker"))?;
    let tool = bridge.builtins.get(id).ok_or_else(|| anyhow::anyhow!("unknown built-in tool"))?;
    Ok(tool.definition.execute.clone())
}

/// `prepareArguments` of a built-in tool named by a `$piRust` marker.
pub(crate) fn rust_prepare_arguments(bridge: &Arc<Bridge>, marker: &Value) -> Option<PrepareArgumentsFn> {
    let id = marker["tool"].as_u64()?;
    bridge.builtins.get(id)?.definition.prepare_arguments
}

fn render_args(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<(ToolDefinition, Arc<crate::modes::interactive::theme::theme::Theme>, ToolRenderContext)> {
    let id = payload["id"].as_u64().unwrap_or_default();
    let tool = bridge.builtins.get(id).ok_or_else(|| anyhow::anyhow!("unknown built-in tool"))?;
    let theme = payload["theme"]
        .as_u64()
        .and_then(|id| bridge.components.themes.get(id))
        .unwrap_or_else(crate::modes::interactive::theme::theme::theme);
    let ctx = payload["context"]
        .as_u64()
        .and_then(|id| bridge.components.render_contexts.get(id))
        .ok_or_else(|| anyhow::anyhow!("built-in renderers need the render context they were called with"))?;
    Ok((tool.definition, theme, ctx))
}

fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

pub(crate) fn dispatch(bridge: &Arc<Bridge>, op: &str, payload: Value) -> anyhow::Result<Value> {
    match op {
        "builtin.create" => {
            let kind = str_arg(&payload, "kind");
            let cwd = payload["cwd"].as_str().map(str::to_string).unwrap_or_else(|| bridge.cwd.clone());
            let definition = create_definition(bridge, &kind, &cwd, &payload["options"])?;
            Ok(describe_definition(bridge, definition))
        }
        "builtin.renderCall" => {
            let (definition, theme, ctx) = render_args(bridge, &payload)?;
            let f = definition.render_call.ok_or_else(|| anyhow::anyhow!("tool has no renderCall"))?;
            Ok(json!(component::register_rust_component(bridge, f(&payload["args"], &theme, &ctx))))
        }
        "builtin.renderResult" => {
            let (definition, theme, ctx) = render_args(bridge, &payload)?;
            let f = definition.render_result.ok_or_else(|| anyhow::anyhow!("tool has no renderResult"))?;
            let result: pi_agent::types::AgentToolResult = serde_json::from_value(payload["result"].clone()).unwrap_or_default();
            let options: ToolRenderResultOptions = opt(&payload["options"])?.unwrap_or_default();
            Ok(json!(component::register_rust_component(bridge, f(&result, options, &theme, &ctx))))
        }
        "ca.config" => Ok(json!({
            "configDirName": crate::config::CONFIG_DIR_NAME,
            "version": crate::config::VERSION,
            "agentDir": crate::config::get_agent_dir(),
            "docsPath": crate::config::get_docs_path(),
            "examplesPath": crate::config::get_examples_path(),
            "packageDir": crate::config::get_package_dir(),
            "readmePath": crate::config::get_readme_path(),
        })),
        "ca.constants" => Ok(json!({
            "DEFAULT_MAX_BYTES": crate::core::tools::truncate::DEFAULT_MAX_BYTES,
            "DEFAULT_MAX_LINES": crate::core::tools::truncate::DEFAULT_MAX_LINES,
        })),
        "ca.theme" => Ok(json!(component::theme_id(bridge, &crate::modes::interactive::theme::theme::theme()))),
        "ca.platform" => Ok(json!(platform())),
        "ca" => helper(bridge, &payload),
        _ => Err(anyhow::anyhow!("unknown host call {op}")),
    }
}

fn helper(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    use crate::core::tools::truncate;
    let f = str_arg(payload, "fn");
    let args = payload.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
    let a = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
    let s = |i: usize| a(i).as_str().unwrap_or_default().to_string();
    Ok(match f.as_str() {
        "formatSize" => json!(truncate::format_size(a(0).as_f64().unwrap_or(0.0) as i64)),
        "truncateHead" => {
            let options: Option<truncate::TruncationOptions> = opt(&a(1))?;
            serde_json::to_value(truncate::truncate_head(&s(0), options.as_ref()))?
        }
        "truncateTail" => {
            let options: Option<truncate::TruncationOptions> = opt(&a(1))?;
            serde_json::to_value(truncate::truncate_tail(&s(0), options.as_ref()))?
        }
        "truncateLine" => serde_json::to_value(truncate::truncate_line(&s(0), a(1).as_f64().map(|v| v as i64)))?,
        "formatReadLineRange" => json!(crate::core::tools::render_utils::format_read_line_range_ref(
            args.first().filter(|v| !v.is_null()),
            args.get(1).filter(|v| !v.is_null())
        )),
        "parseFrontmatter" => {
            let parsed = crate::utils::frontmatter::parse_frontmatter(&s(0))?;
            json!({ "frontmatter": parsed.frontmatter, "body": parsed.body })
        }
        "stripFrontmatter" => json!(crate::utils::frontmatter::strip_frontmatter(&s(0))?),
        "convertToLlm" => {
            let messages: Vec<pi_agent::types::AgentMessage> = serde_json::from_value(a(0))?;
            serde_json::to_value(crate::core::messages::convert_to_llm(&messages))?
        }
        "serializeConversation" => {
            let messages: Vec<pi_ai::types::Message> = serde_json::from_value(a(0))?;
            json!(crate::core::compaction::utils::serialize_conversation(&messages))
        }
        "estimateTokens" => {
            let message: pi_agent::types::AgentMessage = serde_json::from_value(a(0))?;
            json!(crate::core::compaction::compaction::estimate_tokens(&message))
        }
        "getLanguageFromPath" => json!(crate::modes::interactive::theme::theme::get_language_from_path(&s(0))),
        "highlightCode" => json!(crate::modes::interactive::theme::theme::highlight_code(&s(0), a(1).as_str())),
        "markdownHighlightCode" => {
            let theme = crate::modes::interactive::theme::theme::get_markdown_theme();
            match &theme.highlight_code {
                Some(f) => json!(f(&s(0), a(1).as_str())),
                None => json!(s(0).split('\n').collect::<Vec<_>>()),
            }
        }
        "generateDiffString" => serde_json::to_value(crate::core::tools::edit_diff::generate_diff_string(
            &s(0),
            &s(1),
            a(2).as_f64().map(|v| v as i64),
        ))?,
        "initTheme" => {
            crate::modes::interactive::theme::theme::init_theme(a(0).as_str(), a(1).as_bool());
            Value::Null
        }
        "newTheme" => {
            use crate::modes::interactive::theme::theme::{Theme, ThemeBgColors, ThemeFgColors, ThemeOptions};
            let fg: ThemeFgColors = serde_json::from_value(a(0))?;
            let bg: ThemeBgColors = serde_json::from_value(a(1))?;
            let mode = match a(2).as_str() {
                Some("truecolor") => pi_tui::colors::TerminalColorMode::Truecolor,
                _ => pi_tui::colors::TerminalColorMode::Color256,
            };
            let options: Option<ThemeOptions> = opt(&a(3))?;
            let theme = Arc::new(Theme::try_new(&fg, &bg, mode, options.as_ref())?);
            json!(component::theme_id(bridge, &theme))
        }
        "parseSkillBlock" => serde_json::to_value(crate::core::agent_session::parse_skill_block(&s(0)))?,
        "mutationQueueKey" => {
            let resolved = crate::utils::paths::node_resolve(&[&bridge.cwd, &s(0)]);
            json!(std::fs::canonicalize(&resolved).map(|p| p.to_string_lossy().into_owned()).unwrap_or(resolved))
        }
        "builtinPrepareArguments" => {
            let id = a(0).as_u64().unwrap_or_default();
            match bridge.builtins.get(id).and_then(|t| t.definition.prepare_arguments) {
                Some(prepare) => prepare(a(1)),
                None => a(1),
            }
        }
        _ => return session_helper(&f, &args),
    })
}

/// `leafId` argument of the session-context builders: `{ set: false }` is TS `undefined`
/// (last entry), `{ set: true, id: null }` is `null` (before any entry).
fn leaf_arg(value: &Value) -> Option<Option<String>> {
    if value.get("set").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    Some(value.get("id").and_then(Value::as_str).map(str::to_string))
}

fn file_entry_json(entry: &crate::core::session_manager::FileEntry) -> Value {
    serde_json::to_value(entry).unwrap_or(Value::Null)
}

/// Compaction, session-tree, skill, diff, shell and CLI helpers of `@earendil-works/pi-coding-agent`,
/// backed by their Rust ports.
fn session_helper(f: &str, args: &[Value]) -> anyhow::Result<Value> {
    use crate::core::compaction::{branch_summarization, compaction};
    use crate::core::session_manager::{self as sm, FileEntry, SessionEntry};
    let a = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
    let s = |i: usize| a(i).as_str().unwrap_or_default().to_string();
    let n = |i: usize| a(i).as_f64().unwrap_or(0.0);
    let entries = |i: usize| -> anyhow::Result<Vec<SessionEntry>> { Ok(serde_json::from_value(a(i))?) };
    Ok(match f {
        "calculateContextTokens" => {
            let usage: pi_ai::types::Usage = serde_json::from_value(a(0))?;
            json!(compaction::calculate_context_tokens(&usage))
        }
        "getLastAssistantUsage" => serde_json::to_value(compaction::get_last_assistant_usage(&entries(0)?))?,
        "findTurnStartIndex" => json!(compaction::find_turn_start_index(&entries(0)?, n(1) as i64, n(2) as i64)),
        "findCutPoint" => serde_json::to_value(compaction::find_cut_point(&entries(0)?, n(1) as i64, n(2) as i64, n(3).max(0.0) as u64))?,
        "shouldCompact" => {
            let settings: compaction::CompactionSettings = serde_json::from_value(a(2))?;
            json!(compaction::should_compact(n(0).max(0.0) as u64, n(1).max(0.0) as u64, &settings))
        }
        "prepareBranchEntries" => serde_json::to_value(branch_summarization::prepare_branch_entries(
            &entries(0)?,
            a(1).as_f64().map(|v| v as i64),
        ))?,
        "buildContextEntries" | "buildSessionProjection" | "buildSessionContext" => {
            let list = entries(0)?;
            let leaf = leaf_arg(&a(1));
            let leaf = leaf.as_ref().map(|l| l.as_deref());
            match f {
                "buildContextEntries" => serde_json::to_value(sm::build_context_entries(&list, leaf, None))?,
                "buildSessionProjection" => serde_json::to_value(sm::build_session_projection(&list, leaf, None))?,
                _ => serde_json::to_value(sm::build_session_context(&list, leaf, None))?,
            }
        }
        "getLatestCompactionEntry" => serde_json::to_value(sm::get_latest_compaction_entry(&entries(0)?))?,
        "migrateSessionEntries" => {
            let mut list = a(0)
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(FileEntry::from_value)
                .collect::<Result<Vec<_>, _>>()?;
            sm::migrate_session_entries(&mut list);
            Value::Array(list.iter().map(file_entry_json).collect())
        }
        "parseSessionEntries" => Value::Array(sm::parse_session_entries(&s(0)).iter().map(file_entry_json).collect()),
        "sessionEntryToContextMessages" => {
            let entry: SessionEntry = serde_json::from_value(a(0))?;
            serde_json::to_value(sm::session_entry_to_context_messages(&entry))?
        }
        "renderDiff" => json!(crate::modes::interactive::components::diff::render_diff(&s(0), None)),
        "truncateToVisualLines" => {
            let keep = opt(&a(4))?;
            serde_json::to_value(crate::modes::interactive::components::visual_truncate::truncate_to_visual_lines(
                &s(0),
                n(1) as i64,
                n(2) as i64,
                a(3).as_f64().map(|v| v as i64),
                keep,
            ))?
        }
        "generateUnifiedPatch" => json!(crate::core::tools::edit_diff::generate_unified_patch(
            &s(0),
            &s(1),
            &s(2),
            a(3).as_f64().map(|v| v as i64)
        )),
        "formatDimensionNote" => {
            let result: crate::utils::image_resize_core::ResizedImage = serde_json::from_value(a(0))?;
            json!(crate::utils::image_resize::format_dimension_note(&result))
        }
        "getShellConfig" => serde_json::to_value(crate::utils::shell::get_shell_config(a(0).as_str())?)?,
        "getPowerShellConfig" => serde_json::to_value(crate::utils::shell::get_power_shell_config()?)?,
        "formatSkillsForPrompt" => {
            let skills: Vec<crate::core::skills::Skill> = serde_json::from_value(a(0))?;
            json!(crate::core::skills::format_skills_for_prompt(&skills, opt(&a(1))?))
        }
        "loadSkills" => serde_json::to_value(crate::core::skills::load_skills(&serde_json::from_value(a(0))?))?,
        "loadSkillsFromDir" => serde_json::to_value(crate::core::skills::load_skills_from_dir(&serde_json::from_value(a(0))?))?,
        "loadProjectContextFiles" => {
            serde_json::to_value(crate::core::resource_loader::load_project_context_files(&serde_json::from_value(a(0))?))?
        }
        "hasTrustRequiringProjectResources" => json!(crate::core::trust_manager::has_trust_requiring_project_resources(&s(0))),
        "readStoredCredential" => serde_json::to_value(crate::core::auth_storage::read_stored_credential(&s(0), a(1).as_str()))?,
        "parseArgs" => {
            let argv: Vec<String> = serde_json::from_value(a(0))?;
            serde_json::to_value(crate::cli::args::parse_args(&argv))?
        }
        _ => return Err(anyhow::anyhow!("{f} is not available to extensions in pi-rs")),
    })
}

/// `compact()`, `generateSummary()`, `generateSummaryWithUsage()` and `generateBranchSummary()`:
/// the Rust compaction engine with the extension's model, auth, signal, stream function and
/// retry callbacks.
async fn compaction_helper(bridge: &Arc<Bridge>, p: &Value) -> anyhow::Result<Value> {
    use crate::core::compaction::{branch_summarization, compaction};
    use pi_ai::utils::retry::{RetryCallbacks, RetryPolicy};
    let model: pi_ai::types::Model = serde_json::from_value(p["model"].clone())?;
    let api_key = p["apiKey"].as_str().map(str::to_string);
    let headers: Option<indexmap::IndexMap<String, String>> = opt(&p["headers"])?;
    let env: Option<indexmap::IndexMap<String, String>> = opt(&p["env"])?;
    let retry: Option<RetryPolicy> = opt(&p["retry"])?;
    let thinking: Option<pi_agent::types::ThinkingLevel> = opt(&p["thinkingLevel"])?;
    let signal = bridge.token(&p["signal"]);
    let custom = p["customInstructions"].as_str().map(str::to_string);
    let session_id = p["sessionId"].as_str().map(str::to_string);
    let stream_fn: Option<pi_agent::types::StreamFn> = bridge.handle(&p["streamFn"]).map(|handle| {
        let bridge = bridge.clone();
        Arc::new(
            move |model: pi_ai::types::Model,
                  context: pi_ai::types::TranscriptContext,
                  options: pi_ai::types::SimpleStreamOptions| {
                let context = serde_json::to_value(&context).unwrap_or(Value::Null);
                let stream = super::native::js_stream(bridge.clone(), handle.clone(), None, model, vec![context], options);
                Box::pin(async move { Ok(stream) }) as futures::future::BoxFuture<'static, _>
            },
        ) as pi_agent::types::StreamFn
    });
    let callbacks = {
        let cb = &p["callbacks"];
        let f = |name: &str| bridge.handle(&cb[name]).map(|h| js_async_fn(bridge, h));
        let (scheduled, start, finished) = (f("onRetryScheduled"), f("onRetryAttemptStart"), f("onRetryFinished"));
        let any = scheduled.is_some() || start.is_some() || finished.is_some();
        any.then(|| RetryCallbacks {
            on_retry_scheduled: scheduled.map(|f| {
                Box::new(move |attempt: u32, max: u32, delay: u64, error: String| {
                    let fut = f(vec![json!(attempt), json!(max), json!(delay), json!(error)]);
                    Box::pin(async move {
                        let _ = fut.await;
                    }) as futures::future::BoxFuture<'static, ()>
                }) as pi_ai::utils::retry::OnRetryScheduled
            }),
            on_retry_attempt_start: start.map(|f| {
                Box::new(move || {
                    let fut = f(vec![]);
                    Box::pin(async move {
                        let _ = fut.await;
                    }) as futures::future::BoxFuture<'static, ()>
                }) as pi_ai::utils::retry::OnRetryAttemptStart
            }),
            on_retry_finished: finished.map(|f| {
                Box::new(move |success: bool, attempt: u32, error: Option<String>| {
                    let fut = f(vec![json!(success), json!(attempt), json!(error)]);
                    Box::pin(async move {
                        let _ = fut.await;
                    }) as futures::future::BoxFuture<'static, ()>
                }) as pi_ai::utils::retry::OnRetryFinished
            }),
        })
    };
    Ok(match p["fn"].as_str().unwrap_or_default() {
        "compact" => {
            let preparation: compaction::CompactionPreparation = serde_json::from_value(p["preparation"].clone())?;
            serde_json::to_value(
                compaction::compact(
                    &preparation,
                    &model,
                    api_key.as_deref(),
                    headers.as_ref(),
                    custom.as_deref(),
                    signal,
                    thinking,
                    stream_fn,
                    env.as_ref(),
                    retry.as_ref(),
                    callbacks.as_ref(),
                    session_id.as_deref(),
                )
                .await?,
            )?
        }
        f @ ("generateSummary" | "generateSummaryWithUsage") => {
            let messages: Vec<pi_agent::types::AgentMessage> = serde_json::from_value(p["messages"].clone())?;
            let result = compaction::generate_summary_with_usage(
                &messages,
                &model,
                p["reserveTokens"].as_f64().map(|v| v as u64),
                api_key.as_deref(),
                headers.as_ref(),
                signal,
                custom.as_deref(),
                p["previousSummary"].as_str(),
                thinking,
                stream_fn,
                env.as_ref(),
                retry.as_ref(),
                callbacks.as_ref(),
                session_id.as_deref(),
            )
            .await?;
            if f == "generateSummary" { json!(result.text) } else { serde_json::to_value(result)? }
        }
        "generateBranchSummary" => {
            let entries: Vec<crate::core::session_manager::SessionEntry> = serde_json::from_value(p["entries"].clone())?;
            let options = branch_summarization::GenerateBranchSummaryOptions {
                model,
                api_key,
                headers,
                env,
                signal: signal.unwrap_or_default(),
                custom_instructions: custom,
                replace_instructions: p["replaceInstructions"].as_bool(),
                reserve_tokens: p["reserveTokens"].as_f64().map(|v| v as u64),
                stream_fn,
                retry,
                callbacks,
            };
            serde_json::to_value(branch_summarization::generate_branch_summary(&entries, &options).await?)?
        }
        other => return Err(anyhow::anyhow!("{other} is not available to extensions in pi-rs")),
    })
}

/// Async image helpers (`resizeImage`, `convertToPng`, `detectSupportedImageMimeTypeFromFile`).
async fn image_helper(payload: &Value) -> anyhow::Result<Value> {
    let f = str_arg(payload, "fn");
    let args = payload.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
    let a = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
    let s = |i: usize| a(i).as_str().unwrap_or_default().to_string();
    Ok(match f.as_str() {
        "resizeImage" => {
            let options: Option<crate::utils::image_resize_core::ImageResizeOptions> = opt(&a(2))?;
            serde_json::to_value(crate::utils::image_resize::resize_image(&bytes_of(&a(0)), &s(1), options.as_ref()).await?)?
        }
        "convertToPng" => serde_json::to_value(crate::utils::image_convert::convert_to_png(&s(0), &s(1)).await?)?,
        "detectSupportedImageMimeTypeFromFile" => {
            json!(crate::utils::mime::detect_supported_image_mime_type_from_file(&s(0)).await?)
        }
        _ => return Err(anyhow::anyhow!("{f} is not available to extensions in pi-rs")),
    })
}

pub(crate) async fn dispatch_async(bridge: Arc<Bridge>, op: &str, payload: Value) -> anyhow::Result<Value> {
    match op {
        "builtin.execute" => {
            let id = payload["id"].as_u64().unwrap_or_default();
            let tool = bridge.builtins.get(id).ok_or_else(|| anyhow::anyhow!("unknown built-in tool"))?;
            let tool_call_id = str_arg(&payload, "toolCallId");
            let signal = bridge.token(&payload["signal"]);
            let on_update = bridge.handle(&payload["onUpdate"]).map(|h| {
                Arc::new(move |partial: pi_agent::types::AgentToolResult| {
                    super::api::post_call(&h, None, vec![serde_json::to_value(&partial).unwrap_or(Value::Null)]);
                }) as pi_agent::types::AgentToolUpdateCallback
            });
            let ctx = payload["ctx"].as_u64().and_then(|id| bridge.contexts.get(id)).and_then(|entry| match entry {
                CtxEntry::Tool(t) => Some(t),
                other => other.base().map(|base| ExtensionToolContext {
                    ctx: base.clone(),
                    tool_call_id: tool_call_id.clone(),
                    signal: signal.clone(),
                }),
            });
            let result = (tool.definition.execute)(tool_call_id, payload["params"].clone(), signal, on_update, ctx).await?;
            Ok(serde_json::to_value(result)?)
        }
        "ca.localBashExec" => {
            let options = CreateLocalBashOperationsOptions { shell_path: payload["shellPath"].as_str().map(str::to_string) };
            let ops = create_local_bash_operations(Some(&options));
            let exec_options = &payload["options"];
            let on_data = bridge.handle(&exec_options["onData"]);
            let result = (ops.exec)(
                str_arg(&payload, "command"),
                str_arg(&payload, "cwd"),
                crate::core::tools::bash::BashOperationsOptions {
                    on_data: Arc::new(move |bytes: Vec<u8>| {
                        if let Some(h) = &on_data {
                            super::api::post_call(h, None, vec![json!({ "$bytes": bytes })]);
                        }
                    }),
                    signal: bridge.token(&exec_options["signal"]),
                    timeout: exec_options["timeout"].as_f64(),
                    env: opt(&exec_options["env"])?,
                },
            )
            .await?;
            Ok(serde_json::to_value(result)?)
        }
        "ca.copyToClipboard" => {
            crate::utils::clipboard::copy_to_clipboard(&str_arg(&payload, "text")).await?;
            Ok(Value::Null)
        }
        "ca.image" => image_helper(&payload).await,
        "ca.compaction" => compaction_helper(&bridge, &payload).await,
        _ => {
            let _ = arg(&payload, 0);
            Err(anyhow::anyhow!("unknown async host call {op}"))
        }
    }
}

/// A Rust `ToolDefinition` for JavaScript: the extension's own tool object when it registered the
/// tool from this runtime (`{ object }`), otherwise a description of the Rust tool that
/// `toolFromHost` (pi-sdk.js) wraps.
pub(crate) fn tool_definition_to_js(bridge: &Arc<Bridge>, definition: ToolDefinition) -> Value {
    if let Some(origin) = bridge.origins.get(&definition.execute) {
        return json!({ "object": origin });
    }
    describe_definition(bridge, definition)
}

/// Register a Rust tool for JavaScript and describe it (`builtinTool` in pi-coding-agent.js).
fn describe_definition(bridge: &Arc<Bridge>, definition: ToolDefinition) -> Value {
    let mut out = json!({
        "name": definition.name,
        "label": definition.label,
        "description": definition.description,
        "parameters": definition.parameters,
        "promptSnippet": definition.prompt_snippet,
        "promptGuidelines": definition.prompt_guidelines,
        "renderShell": serde_json::to_value(definition.render_shell).unwrap_or(Value::Null),
        "executionMode": serde_json::to_value(definition.execution_mode).unwrap_or(Value::Null),
        "hasPrepareArguments": definition.prepare_arguments.is_some(),
        "hasRenderCall": definition.render_call.is_some(),
        "hasRenderResult": definition.render_result.is_some(),
    });
    if let Some(a) = definition.default_active {
        out["defaultActive"] = json!(a);
    }
    if let Some(x) = &definition.exposure {
        out["exposure"] = serde_json::to_value(x).unwrap_or(Value::Null);
    }
    let id = bridge.builtins.insert(BuiltinTool { definition });
    out["id"] = json!(id);
    out
}
