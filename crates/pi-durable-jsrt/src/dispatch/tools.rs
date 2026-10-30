//! `@earendil-works/pi-durable/tools` (`js/tools/index.js`) and the tool helpers its sources export
//! (`truncate.ts`, `tools/image.ts`, `tools/image-processor.ts`, `tools/file-mutation-queue.ts`):
//! the built-in `read`, `write`, `edit`, `bash` and `powershell` tools are `pi_durable::tools`'s
//! (`builtin.*`), running against the call's JS `ExecutionEnv` and `ToolExecutionApi`. Re-hosts
//! `pi-durable/examples/support/builtin_bridge.rs`.

use std::sync::Arc;

use pi_durable::env::{ExecutionEnv, ShellOutputSkip, ShellOutputWindow};
use pi_durable::harness::live::ToolDiagnostic;
use pi_durable::harness::tool::RetainedOutput;
use pi_durable::storage::StorageFuture;
use pi_durable::tools::api::CodingToolApi;
use pi_durable::tools::bash::{
    BashExecution, BashPrepare, BashToolInput, BashToolOptions, execute_shell,
};
use pi_durable::tools::edit::{create_edit_tool, execute_edit, prepare_edit_arguments};
use pi_durable::tools::file_mutation_queue::with_file_mutation_queue;
use pi_durable::tools::image_processor::{DEFAULT_IMAGE_LIMITS, ImageLimits, ImageModel, Images};
use pi_durable::tools::read::{ReadToolInput, ReadToolOptions, create_read_tool, execute_read};
use pi_durable::tools::write::{create_write_tool, execute_write};
use pi_durable::tools::{create_bash_tool, create_powershell_tool};
use pi_durable::truncate::{TruncationOptions, format_size, truncate_head, truncate_head_of};
use pi_durable::{Context, ErrorKind, JsonString};
use pi_jsrt::JsHandle;

use crate::dispatch::env::{
    call_js, execution_error, file_error, js_env, native_context, optional_number,
};
use crate::dispatch::images::images_from;
use crate::errors::{ErrorClass, HostError, HostResult};
use crate::host::{Args, Dispatch, Methods, call_async};
use crate::value::HostValue;

/// The tools method group.
pub struct ToolsDispatch;

/// A `pi_durable::Error` as the JS error upstream's tools throw.
pub fn durable_error(error: &pi_durable::Error) -> HostError {
    match error.kind() {
        ErrorKind::File(e) => file_error(e),
        ErrorKind::Execution(e) => execution_error(e),
        ErrorKind::Caused { message, cause } => {
            HostError::typed(ErrorClass::Error, message.clone()).with_cause(durable_error(cause))
        }
        ErrorKind::Host { name, message } => match ErrorClass::from_name(name) {
            Some(class) => HostError::typed(class, message.clone()),
            None => HostError::typed(ErrorClass::Error, message.clone())
                .with_prop("name", name.as_str()),
        },
        _ => HostError::typed(ErrorClass::Error, error.message_text()),
    }
}

fn host_failure(error: HostError) -> pi_durable::Error {
    pi_durable::Error::host(error.name().to_string(), error.message().clone())
}

fn strict<T: serde::Serialize + ?Sized>(value: &T) -> HostValue {
    HostValue::from_strict(&crate::dispatch::env::encode(value))
}

fn decode<T: serde::de::DeserializeOwned>(value: &HostValue) -> HostResult<T> {
    let value = value.to_strict_omitting_undefined()?;
    crate::dispatch::env::decode(&value).map_err(HostError::type_error)
}

/// The call's `ToolExecutionApi` (through the per-call port `js/tools/index.js` builds).
struct JsApi {
    port: JsHandle,
    env: Option<Arc<dyn ExecutionEnv>>,
    window: Option<ShellOutputWindow>,
}

impl JsApi {
    fn new(port: JsHandle, description: &HostValue) -> HostResult<Self> {
        let window = match description.get("window") {
            Some(window @ HostValue::Object(_)) => {
                let n = |key: &str| window.get(key).and_then(HostValue::as_f64).unwrap_or(0.0);
                Some(ShellOutputWindow {
                    max_bytes: n("maxBytes").max(0.0) as usize,
                    max_lines: n("maxLines").max(0.0) as usize,
                    min_interval_ms: n("minIntervalMs"),
                    bytes_per_second: n("bytesPerSecond"),
                })
            }
            _ => None,
        };
        Ok(JsApi {
            port,
            env: js_env(description.get("env").unwrap_or(&HostValue::Undefined))?,
            window,
        })
    }
    fn call(&self, method: &str, args: Vec<HostValue>) -> pi_durable::Result<HostValue> {
        call_js(&self.port, Some(method), args).map_err(host_failure)
    }
}

impl CodingToolApi for JsApi {
    fn environment(&self) -> pi_durable::Result<Option<Arc<dyn ExecutionEnv>>> {
        Ok(self.env.clone())
    }
    fn output(&self, text: JsonString, skipped: Option<ShellOutputSkip>) -> pi_durable::Result<()> {
        let skipped = skipped
            .map(|s| {
                HostValue::object([
                    ("bytes", HostValue::from(s.bytes as f64)),
                    ("newlines", HostValue::from(s.newlines as f64)),
                    ("endsWithNewline", HostValue::from(s.ends_with_newline)),
                ])
            })
            .unwrap_or_default();
        self.call("output", vec![HostValue::String(text), skipped])
            .map(drop)
    }
    fn retained_output(&self) -> pi_durable::Result<RetainedOutput> {
        let value = self.call("retainedOutput", vec![])?;
        Ok(RetainedOutput {
            text: value
                .get("text")
                .and_then(HostValue::as_string)
                .cloned()
                .unwrap_or_default(),
            truncated: value.get("truncated").and_then(HostValue::as_bool) == Some(true),
        })
    }
    fn diagnostic(&self, diagnostic: ToolDiagnostic) -> pi_durable::Result<()> {
        self.call("diagnostic", vec![strict(&diagnostic)]).map(drop)
    }
    fn output_window(&self) -> pi_durable::Result<Option<ShellOutputWindow>> {
        Ok(self.window)
    }
    fn image_model(&self, _context: &Context) -> StorageFuture<ImageModel> {
        let port = self.port.clone();
        Box::pin(async move {
            let value = call_async(&port, Some("imageModel"), vec![])
                .await
                .map_err(host_failure)?;
            let limit =
                |key: &str, default: f64| optional_number(value.get(key)).unwrap_or(default);
            Ok(ImageModel {
                vision: value.get("vision").and_then(HostValue::as_bool) != Some(false),
                limits: ImageLimits {
                    max_width: limit("maxWidth", DEFAULT_IMAGE_LIMITS.max_width),
                    max_height: limit("maxHeight", DEFAULT_IMAGE_LIMITS.max_height),
                    max_bytes: limit("maxBytes", DEFAULT_IMAGE_LIMITS.max_bytes),
                },
            })
        })
    }
}

fn read_options(images: &HostValue) -> HostResult<ReadToolOptions> {
    Ok(ReadToolOptions {
        images: images_from(images)?,
    })
}

/// Bash options: `{ commandPrefix?, prepare? }`; `prepare` runs through the call's port.
fn bash_options(options: &HostValue, port: &JsHandle) -> HostResult<BashToolOptions> {
    let command_prefix = options
        .get("commandPrefix")
        .and_then(HostValue::as_string)
        .cloned();
    let prepare = match options.get("prepare") {
        Some(HostValue::Handle(_)) => {
            let port = port.clone();
            Some(Arc::new(
                move |execution: BashExecution, _api: Arc<dyn CodingToolApi>, _context: Context| {
                    let port = port.clone();
                    Box::pin(async move {
                        let prepared = call_async(&port, Some("prepare"), vec![strict(&execution)])
                            .await
                            .map_err(host_failure)?;
                        let prepared = prepared
                            .to_strict_omitting_undefined()
                            .map_err(host_failure)?;
                        crate::dispatch::env::decode(&prepared).map_err(pi_durable::Error::message)
                    }) as StorageFuture<BashExecution>
                },
            ) as BashPrepare)
        }
        _ => None,
    };
    Ok(BashToolOptions {
        command_prefix,
        prepare,
    })
}

fn declaration(name: &str, images: bool) -> HostResult<HostValue> {
    let tool = match name {
        "read" => create_read_tool(ReadToolOptions {
            images: images.then(|| pi_durable::images::create_photon_images() as Images),
        }),
        "write" => create_write_tool(),
        "edit" => create_edit_tool(),
        "bash" => create_bash_tool(Default::default()),
        "powershell" => create_powershell_tool(Default::default()),
        _ => {
            return Err(HostError::type_error(format!(
                "unknown built-in tool {name}"
            )));
        }
    };
    let mut fields = vec![
        ("name", HostValue::String(tool.declaration.name.clone())),
        (
            "description",
            HostValue::String(tool.declaration.description.clone()),
        ),
        (
            "parameters",
            HostValue::from_strict(&tool.declaration.parameters),
        ),
    ];
    if let Some(schema) = &tool.structured_output_schema {
        fields.push(("structuredOutputSchema", HostValue::from_strict(schema)));
    }
    if tool.output_limits.retain == pi_chord::output::Retain::Tail {
        fields.push((
            "outputLimits",
            HostValue::object([("retain", HostValue::from("tail"))]),
        ));
    }
    Ok(HostValue::object(fields))
}

fn result(
    r: pi_durable::Result<pi_durable::harness::tool::ToolExecutionResult>,
) -> HostResult<HostValue> {
    r.map(|r| strict(&r)).map_err(|e| durable_error(&e))
}

impl Dispatch for ToolsDispatch {
    fn register(&self, m: &mut Methods) {
        m.sync("builtin.describe", |_, args: Args| {
            declaration(&args.text(0)?, args.get(1).as_bool() == Some(true))
        });
        m.sync("builtin.prepareEdit", |_, args: Args| {
            let value = args.get(0).to_strict_omitting_undefined()?;
            Ok(HostValue::from_strict(&prepare_edit_arguments(value)))
        });
        // (args, envDescription, context)
        m.asynchronous("builtin.write", |_, args: Args| async move {
            let env = js_env(args.get(1))?;
            result(execute_write(decode(args.get(0))?, env, native_context(args.get(2))).await)
        });
        m.asynchronous("builtin.edit", |_, args: Args| async move {
            let env = js_env(args.get(1))?;
            let input = args.get(0).to_strict_omitting_undefined()?;
            result(execute_edit(input, env, native_context(args.get(2))).await)
        });
        // (args, port, apiDescription, images, context)
        m.asynchronous("builtin.read", |_, args: Args| async move {
            let input = args.get(0);
            let input = ReadToolInput {
                path: input
                    .get("path")
                    .and_then(HostValue::as_string)
                    .cloned()
                    .ok_or_else(|| HostError::type_error("path must be a string"))?,
                offset: optional_number(input.get("offset")),
                limit: optional_number(input.get("limit")),
            };
            let api = Arc::new(JsApi::new(args.handle(1)?, args.get(2))?);
            let options = read_options(args.get(3))?;
            result(execute_read(input, options, api, native_context(args.get(4))).await)
        });
        // (args, port, apiDescription, options, context, programs?)
        for name in ["builtin.bash", "builtin.powershell"] {
            let powershell = name == "builtin.powershell";
            m.asynchronous(name, move |_, args: Args| async move {
                let input = args.get(0);
                let input = BashToolInput {
                    command: input
                        .get("command")
                        .and_then(HostValue::as_string)
                        .cloned()
                        .ok_or_else(|| HostError::type_error("command must be a string"))?,
                    timeout: optional_number(input.get("timeout")),
                };
                let port = args.handle(1)?;
                let api = Arc::new(JsApi::new(port.clone(), args.get(2))?);
                let options = bash_options(args.get(3), &port)?;
                let programs = powershell.then(|| match args.get(3).get("programs") {
                    Some(HostValue::Array(programs)) => programs
                        .iter()
                        .filter_map(HostValue::as_string)
                        .cloned()
                        .collect(),
                    _ => vec![JsonString::from("pwsh"), JsonString::from("powershell")],
                });
                result(
                    execute_shell(input, options, programs, api, native_context(args.get(4))).await,
                )
            });
        }
        // (envDescription, path, fn, context): the queue admits `fn`; its result or thrown value
        // passes through unchanged.
        m.asynchronous("builtin.mutation", |_, args: Args| async move {
            let env = pi_durable::tools::require_env(js_env(args.get(0))?)
                .map_err(|e| durable_error(&e))?;
            let path = std::path::PathBuf::from(args.text(1)?);
            let run = args.handle(2)?;
            // The queue's own failures map to JS errors; `fn`'s outcome is returned as it settled.
            with_file_mutation_queue(
                env,
                path,
                move || Box::pin(async move { Ok(call_async(&run, None, vec![]).await) }),
                native_context(args.get(3)),
            )
            .await
            .map_err(|e| durable_error(&e))?
        });

        // truncate.ts
        m.sync("truncate.formatSize", |_, args: Args| {
            Ok(HostValue::string(&format_size(
                args.get(0).as_f64().unwrap_or(f64::NAN),
            )))
        });
        m.sync("truncate.head", |_, args: Args| {
            let options = truncation_options(args.get(1));
            Ok(strict(&truncate_head(args.string(0)?, options)))
        });
        m.sync("truncate.headOf", |_, args: Args| {
            let totals = args.get(1);
            let n = |key: &str| {
                totals
                    .get(key)
                    .and_then(HostValue::as_f64)
                    .unwrap_or(f64::NAN)
            };
            let options = truncation_options(args.get(2));
            Ok(strict(&truncate_head_of(
                args.string(0)?,
                n("lines"),
                n("bytes"),
                options,
            )))
        });
    }
}

fn truncation_options(value: &HostValue) -> TruncationOptions {
    TruncationOptions {
        max_bytes: optional_number(value.get("maxBytes")),
        max_lines: optional_number(value.get("maxLines")),
    }
}
