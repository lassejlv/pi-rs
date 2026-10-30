//! Stock test bindings for production coding-tool implementations.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use pi_durable::tools::{
    edit::*, edit_diff::*, file_mutation_queue::with_file_mutation_queue, write::*,
};
use pi_durable::{
    env::{ExecutionEnv, ShellOutputSkip, ShellOutputWindow},
    harness::tool::RetainedOutput,
    tools::{api::CodingToolApi, bash::*, image_processor::*, read::*},
};
fn number(value: &JsonValue) -> Result<f64> {
    value.to_utf8().map_or_else(
        || decode(value),
        |s| {
            s.parse()
                .map_err(|e: std::num::ParseFloatError| Error::message(e.to_string()))
        },
    )
}
fn optional_number(value: &JsonValue) -> Result<Option<f64>> {
    if value.is_null() {
        Ok(None)
    } else {
        number(value).map(Some)
    }
}
fn image_model(value: &JsonValue) -> Result<ImageModel> {
    Ok(ImageModel {
        vision: field(value, "vision")?,
        limits: ImageLimits {
            max_width: optional_number(&value["maxWidth"])?
                .unwrap_or(DEFAULT_IMAGE_LIMITS.max_width),
            max_height: optional_number(&value["maxHeight"])?
                .unwrap_or(DEFAULT_IMAGE_LIMITS.max_height),
            max_bytes: optional_number(&value["maxBytes"])?
                .unwrap_or(DEFAULT_IMAGE_LIMITS.max_bytes),
        },
    })
}
struct DirectApi {
    hub: Arc<Hub>,
    id: u64,
}
impl DirectApi {
    fn call(&self, method: &str, args: JsonValue) -> Result<JsonValue> {
        self.hub.sync(
            "builtin.api",
            object([
                ("api", encode(&self.id)),
                ("method", encode(&method)),
                ("args", args),
            ]),
        )
    }
}
impl CodingToolApi for DirectApi {
    fn environment(&self) -> Result<Option<Arc<dyn ExecutionEnv>>> {
        self.hub
            .described_environment(&self.call("environment", JsonValue::Null)?)
    }
    fn output(&self, text: JsonString, skipped: Option<ShellOutputSkip>) -> Result<()> {
        let skipped = skipped
            .map(|s| {
                object([
                    ("bytes", encode(&s.bytes)),
                    ("newlines", encode(&s.newlines)),
                    ("endsWithNewline", encode(&s.ends_with_newline)),
                ])
            })
            .unwrap_or(JsonValue::Null);
        self.call(
            "output",
            JsonValue::Array(vec![encode(&text), skipped].into()),
        )?;
        Ok(())
    }
    fn retained_output(&self) -> Result<RetainedOutput> {
        let value = self.call("retainedOutput", JsonValue::Null)?;
        Ok(RetainedOutput {
            text: field(&value, "text")?,
            truncated: field(&value, "truncated")?,
        })
    }
    fn diagnostic(&self, diagnostic: pi_durable::harness::live::ToolDiagnostic) -> Result<()> {
        self.call("diagnostic", encode(&diagnostic))?;
        Ok(())
    }
    fn output_window(&self) -> Result<Option<ShellOutputWindow>> {
        let value = self.call("window", JsonValue::Null)?;
        if value.is_null() {
            Ok(None)
        } else {
            Ok(Some(decode(&value)?))
        }
    }
    fn image_model(&self, context: &Context) -> StorageFuture<ImageModel> {
        let operation = self
            .hub
            .call_context("builtin.imageModel", encode(&self.id), context);
        Box::pin(async move { image_model(&operation.await?.unwrap_or(JsonValue::Null)) })
    }
}
struct HostImages {
    hub: Arc<Hub>,
    id: u64,
}
impl ImageProcessor for HostImages {
    fn prepare(
        &self,
        bytes: Vec<u8>,
        mime: String,
        limits: ImageLimits,
    ) -> StorageFuture<Option<PreparedImage>> {
        let operation = self.hub.call(
            "builtin.imagePrepare",
            object([
                ("processor", encode(&self.id)),
                ("bytes", encode(&STANDARD.encode(bytes))),
                ("mime", encode(&mime)),
                ("limits", encode(&limits)),
            ]),
        );
        Box::pin(async move { operation.await?.map(|value| decode(&value)).transpose() })
    }
}
fn read_options(hub: &Arc<Hub>, options: &JsonValue) -> Result<ReadToolOptions> {
    Ok(ReadToolOptions {
        images: field::<Option<u64>>(options, "images")?.map(|id| {
            Arc::new(HostImages {
                hub: hub.clone(),
                id,
            }) as Images
        }),
    })
}
fn bash_options(hub: &Arc<Hub>, api: u64, options: &JsonValue) -> Result<BashToolOptions> {
    let prepare = field::<Option<u64>>(options, "prepare")?.map(|id| {
        let hub = hub.clone();
        Arc::new(
            move |execution: BashExecution, _: Arc<dyn CodingToolApi>, context: Context| {
                let operation = hub.call_context(
                    "builtin.prepare",
                    object([
                        ("options", encode(&id)),
                        ("api", encode(&api)),
                        ("execution", encode(&execution)),
                    ]),
                    &context,
                );
                Box::pin(async move { decode(&operation.await?.unwrap_or(JsonValue::Null)) })
                    as StorageFuture<BashExecution>
            },
        ) as BashPrepare
    });
    Ok(BashToolOptions {
        command_prefix: field(options, "commandPrefix")?,
        prepare,
    })
}

#[derive(Default)]
pub(super) struct BuiltinBridge {
    writes: AtomicU64,
    edits: AtomicU64,
    queues: AtomicU64,
    pure: AtomicU64,
    reads: AtomicU64,
    shells: AtomicU64,
    images: Mutex<HashMap<u64, Arc<pi_durable::images::NativePhotonImages>>>,
    image_preparations: AtomicU64,
}
impl Hub {
    pub(super) fn builtin_request(
        self: &Arc<Self>,
        method: &str,
        request: JsonValue,
        context: Context,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        match method {
            "builtin.modelExports" => {
                use pi_durable::codemode::models::*;
                let value = object([
                    ("description", encode(&MODELS_DESCRIPTION)),
                    ("catalog", encode(&CATALOG_GLOBALS)),
                    ("prefix", encode(&CODEMODE_INTERNAL_PREFIX)),
                    ("classify", encode(&CLASSIFY_TOOL)),
                    ("images", encode(&GENERATE_IMAGES_TOOL)),
                    ("concurrency", encode(&MAX_CONCURRENT_MODEL_CALLS)),
                ]);
                Ok(Box::pin(async move { Ok(Some(value)) }))
            }
            "builtin.stats" => {
                let stats = object([
                    (
                        "writes",
                        encode(&self.builtin.writes.load(Ordering::Relaxed)),
                    ),
                    ("edits", encode(&self.builtin.edits.load(Ordering::Relaxed))),
                    (
                        "queues",
                        encode(&self.builtin.queues.load(Ordering::Relaxed)),
                    ),
                    ("pure", encode(&self.builtin.pure.load(Ordering::Relaxed))),
                    ("reads", encode(&self.builtin.reads.load(Ordering::Relaxed))),
                    (
                        "shells",
                        encode(&self.builtin.shells.load(Ordering::Relaxed)),
                    ),
                    ("imageProcessors", encode(&self.builtin.images.lock().len())),
                    (
                        "imagePreparations",
                        encode(&self.builtin.image_preparations.load(Ordering::Relaxed)),
                    ),
                ]);
                Ok(Box::pin(async { Ok(Some(stats)) }))
            }
            "builtin.imageNew" => {
                let id = self.next.fetch_add(1, Ordering::Relaxed);
                let processor = pi_durable::images::create_native_photon_images(
                    pi_durable::images::NodePhotonImagesOptions {
                        worker: request["worker"].as_bool().unwrap_or(true),
                        idle_ms: optional_number(&request["idleMs"])?.unwrap_or(30000.0),
                    },
                );
                self.builtin.images.lock().insert(id, processor);
                Ok(Box::pin(async move { Ok(Some(encode(&id))) }))
            }
            "builtin.nativeImagePrepare" => {
                self.builtin
                    .image_preparations
                    .fetch_add(1, Ordering::Relaxed);
                let processor = self
                    .builtin
                    .images
                    .lock()
                    .get(&field::<u64>(&request, "processor")?)
                    .cloned()
                    .ok_or_else(|| Error::message("Unknown image processor"))?;
                let bytes = STANDARD
                    .decode(field::<String>(&request, "bytes")?)
                    .map_err(|e| Error::message(e.to_string()))?;
                let l = &request["limits"];
                let limits = ImageLimits {
                    max_width: number(&l["maxWidth"])?,
                    max_height: number(&l["maxHeight"])?,
                    max_bytes: number(&l["maxBytes"])?,
                };
                let operation = processor.prepare(bytes, field(&request, "mime")?, limits);
                Ok(Box::pin(async move {
                    Ok(operation.await?.map(|value| encode(&value)))
                }))
            }
            "builtin.describe" => {
                let tool = match request["tool"].to_utf8().as_deref() {
                    Some("write") => create_write_tool(),
                    Some("edit") => create_edit_tool(),
                    Some("read") => create_read_tool(read_options(self, &request["options"])?),
                    Some("bash") => create_bash_tool(Default::default()),
                    Some("powershell") => create_powershell_tool(Default::default()),
                    Some("tool_search") => pi_durable::tool_search::create_tool_search_tool(),
                    Some(name)
                        if [
                            pi_durable::codemode::models::CLASSIFY_TOOL,
                            pi_durable::codemode::models::GENERATE_IMAGES_TOOL,
                        ]
                        .contains(&name) =>
                    {
                        pi_durable::codemode::models::create_model_tools()
                            .into_iter()
                            .find(|tool| tool.declaration.name.to_string_lossy() == name)
                            .unwrap()
                    }
                    _ => return Err(Error::message("Unknown built-in tool")),
                };
                let mut value = encode(&tool.declaration);
                if let Some(schema) = tool.structured_output_schema {
                    value
                        .as_object_mut()
                        .unwrap()
                        .insert("structuredOutputSchema".into(), schema);
                }
                if tool.output_limits.retain == pi_chord::output::Retain::Tail {
                    value
                        .as_object_mut()
                        .unwrap()
                        .insert("outputLimits".into(), object([("retain", encode(&"tail"))]));
                }
                Ok(Box::pin(async move { Ok(Some(value)) }))
            }
            "builtin.prepareEdit" => {
                let args = prepare_edit_arguments(request["args"].clone());
                Ok(Box::pin(async move { Ok(Some(args)) }))
            }
            "builtin.write" | "builtin.edit" => {
                let env = self.described_environment(&request["environment"])?;
                let operation = if method == "builtin.write" {
                    self.builtin.writes.fetch_add(1, Ordering::Relaxed);
                    execute_write(field(&request, "args")?, env, context)
                } else {
                    self.builtin.edits.fetch_add(1, Ordering::Relaxed);
                    execute_edit(request["args"].clone(), env, context)
                };
                Ok(Box::pin(async move { Ok(Some(encode(&operation.await?))) }))
            }
            "builtin.read" => {
                self.builtin.reads.fetch_add(1, Ordering::Relaxed);
                let args = &request["args"];
                let input = ReadToolInput {
                    path: field(args, "path")?,
                    offset: optional_number(&args["offset"])?,
                    limit: optional_number(&args["limit"])?,
                };
                let api = Arc::new(DirectApi {
                    hub: self.clone(),
                    id: field(&request, "api")?,
                });
                let operation = execute_read(
                    input,
                    read_options(self, &request["options"])?,
                    api,
                    context,
                );
                Ok(Box::pin(async move { Ok(Some(encode(&operation.await?))) }))
            }
            "builtin.bash" | "builtin.powershell" => {
                self.builtin.shells.fetch_add(1, Ordering::Relaxed);
                let id = field(&request, "api")?;
                let options = &request["options"];
                let programs = if method == "builtin.powershell" {
                    Some(
                        field::<Option<Vec<JsonString>>>(options, "programs")?
                            .unwrap_or_else(|| vec!["pwsh".into(), "powershell".into()]),
                    )
                } else {
                    None
                };
                let args = &request["args"];
                let input = BashToolInput {
                    command: field(args, "command")?,
                    timeout: optional_number(&args["timeout"])?,
                };
                let operation = execute_shell(
                    input,
                    bash_options(self, id, options)?,
                    programs,
                    Arc::new(DirectApi {
                        hub: self.clone(),
                        id,
                    }),
                    context,
                );
                Ok(Box::pin(async move { Ok(Some(encode(&operation.await?))) }))
            }
            "builtin.mutation" => {
                self.builtin.queues.fetch_add(1, Ordering::Relaxed);
                let env = pi_durable::tools::require_env(
                    self.described_environment(&request["environment"])?,
                )?;
                let callback = field::<u64>(&request, "callback")?;
                let hub = self.clone();
                let call_context = context.clone();
                Ok(with_file_mutation_queue(
                    env,
                    field(&request, "path")?,
                    move || {
                        Box::pin(async move {
                            hub.call_context("builtin.function", encode(&callback), &call_context)
                                .await
                        })
                    },
                    context,
                ))
            }
            "builtin.pure" => {
                self.builtin.pure.fetch_add(1, Ordering::Relaxed);
                let args = &request["args"];
                let method = field::<String>(&request, "operation")?;
                let result = match method.as_str() {
                    "apply" => {
                        let applied = apply_edits_to_normalized_content(
                            &field(args, "content")?,
                            &field::<Vec<Edit>>(args, "edits")?,
                            &field(args, "path")?,
                        )?;
                        encode(&applied)
                    }
                    "diff" => encode(&generate_diff_string(
                        &field(args, "old")?,
                        &field(args, "new")?,
                        field(args, "context")?,
                    )),
                    "patch" => encode(&generate_unified_patch(
                        &field(args, "path")?,
                        &field(args, "old")?,
                        &field(args, "new")?,
                        field(args, "context")?,
                    )),
                    "fuzzy" => encode(&fuzzy_find_text(
                        &field(args, "content")?,
                        &field(args, "oldText")?,
                    )),
                    "normalize" => encode(&normalize_for_fuzzy_match(&field(args, "text")?)),
                    "image.exif" => {
                        let bytes = STANDARD
                            .decode(field::<String>(args, "bytes")?)
                            .map_err(|e| Error::message(e.to_string()))?;
                        encode(&pi_durable::images::exif::exif_orientation(&bytes))
                    }
                    "image.detect" => {
                        let bytes = STANDARD
                            .decode(field::<String>(args, "bytes")?)
                            .map_err(|e| Error::message(e.to_string()))?;
                        encode(&pi_durable::tools::image::detect_supported_image_mime_type(
                            &bytes,
                        ))
                    }
                    "image.dimensions" => {
                        let bytes = STANDARD
                            .decode(field::<String>(args, "bytes")?)
                            .map_err(|e| Error::message(e.to_string()))?;
                        encode(&pi_durable::tools::image::image_dimensions(
                            &bytes,
                            &field::<String>(args, "mime")?,
                        ))
                    }
                    "base64" => {
                        let bytes = STANDARD
                            .decode(field::<String>(args, "bytes")?)
                            .map_err(|e| Error::message(e.to_string()))?;
                        encode(&to_base64(&bytes))
                    }
                    "base64.length" => encode(&base64_length(number(&args["bytes"])?)),
                    _ => return Err(Error::message("Unknown pure tool operation")),
                };
                Ok(Box::pin(async { Ok(Some(result)) }))
            }
            _ => Err(Error::message("Unknown built-in request")),
        }
    }
}
