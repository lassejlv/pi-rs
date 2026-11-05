//! Test transport for native code-mode components and original host capabilities.
use super::*;
use pi_ai::ModelType;
use pi_durable::codemode::models::*;
use pi_durable::codemode::output::*;
pub(super) struct HostCodemodeModels {
    pub hub: std::sync::Weak<Hub>,
    pub id: u64,
}
impl HostCodemodeModels {
    fn hub(&self) -> Result<Arc<Hub>> {
        self.hub
            .upgrade()
            .ok_or_else(|| Error::message("Model catalog host closed"))
    }
    fn model_handle(&self, value: JsonValue) -> Result<CatalogModel> {
        let id: u64 = field(&value, "handle")?;
        let reference = field(&value, "reference")?;
        let host = self.hub.clone();
        Ok(CatalogModel::new(reference, id, move || {
            host.upgrade()
                .ok_or_else(|| Error::message("Model catalog host closed"))?
                .sync("codemode.models.info", encode(&id))
        }))
    }
    fn asynchronous(
        &self,
        method: &'static str,
        mut args: JsonValue,
        context: Context,
    ) -> StorageFuture<JsonValue> {
        let setup = (|| {
            let hub = self.hub()?;
            let id = hub.next.fetch_add(1, Ordering::Relaxed);
            let signal = context.abort_signal().cloned();
            let object = args.as_object_mut().unwrap();
            object.insert("collection".into(), encode(&self.id));
            object.insert("stream".into(), encode(&id));
            object.insert(
                "aborted".into(),
                encode(&signal.as_ref().is_some_and(|s| s.aborted())),
            );
            let pending = hub.call_context(method, args, &context);
            Ok::<_, Error>((hub, id, signal, pending))
        })();
        Box::pin(async move {
            let (hub, id, signal, pending) = setup?;
            tokio::pin!(pending);
            let reply = if let Some(signal) = signal {
                tokio::select! {reply=&mut pending=>reply,_=signal.cancelled()=>{hub.emit(object([("type",encode(&"codemode.models.signal")),("stream",encode(&id))]));pending.await}}
            } else {
                pending.await
            };
            reply?.ok_or_else(|| Error::message("Model callback returned undefined"))
        })
    }
}
impl CodemodeModels for HostCodemodeModels {
    fn models(&self, kind: ModelType, provider: Option<JsonString>) -> Result<Vec<CatalogModel>> {
        let reply = self.hub()?.sync(
            "codemode.models.list",
            object([
                ("collection", encode(&self.id)),
                ("kind", encode(&kind)),
                ("provider", encode(&provider)),
            ]),
        )?;
        decode::<Vec<JsonValue>>(&reply)?
            .into_iter()
            .map(|value| self.model_handle(value))
            .collect()
    }
    fn available(
        &self,
        kind: ModelType,
        provider: Option<JsonString>,
        context: Context,
    ) -> StorageFuture<Vec<CatalogModel>> {
        let pending = self.asynchronous(
            "codemode.models.available",
            object([("kind", encode(&kind)), ("provider", encode(&provider))]),
            context,
        );
        let host = self.hub.clone();
        let id = self.id;
        Box::pin(async move {
            let models = HostCodemodeModels { hub: host, id };
            decode::<Vec<JsonValue>>(&pending.await?)?
                .into_iter()
                .map(|value| models.model_handle(value))
                .collect()
        })
    }
    fn model(&self, kind: ModelType, reference: &ModelRef) -> Result<Option<CatalogModel>> {
        let reply = self.hub()?.sync(
            "codemode.models.get",
            object([
                ("collection", encode(&self.id)),
                ("kind", encode(&kind)),
                ("reference", encode(reference)),
            ]),
        )?;
        if reply.is_null() {
            Ok(None)
        } else {
            self.model_handle(reply).map(Some)
        }
    }
    fn request(
        &self,
        effect: ModelEffect,
        model: CatalogModel,
        input: JsonValue,
        context: Context,
    ) -> StorageFuture<JsonValue> {
        let setup = (|| {
            let hub = self.hub()?;
            hub.tool.typed_requests.fetch_add(1, Ordering::Relaxed);
            Ok::<_, Error>(*model.value::<u64>()?)
        })();
        match setup {
            Ok(id) => self.asynchronous(
                "codemode.models.request",
                object([
                    ("effect", encode(&effect.kind())),
                    ("model", encode(&id)),
                    ("input", input),
                ]),
                context,
            ),
            Err(error) => Box::pin(async { Err(error) }),
        }
    }
}
impl Hub {
    pub(super) fn codemode_catalog(
        self: &Arc<Self>,
        request: JsonValue,
        context: Context,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        self.tool.catalog_queries.fetch_add(1, Ordering::Relaxed);
        let models = Arc::new(HostCodemodeModels {
            hub: Arc::downgrade(self),
            id: field(&request, "collection")?,
        });
        let name: String = field(&request, "name")?;
        Ok(catalog_call(
            &name,
            request
                .as_object()
                .unwrap()
                .get(&JsonString::from("args"))
                .cloned(),
            models,
            context,
        ))
    }
    pub(super) fn format_codemode(
        self: &Arc<Self>,
        request: JsonValue,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        self.tool.codemode_formats.fetch_add(1, Ordering::Relaxed);
        let options = &request["options"];
        let writer: u64 = field(&request, "writer")?;
        let hub = self.clone();
        let write: OutputFile = Arc::new(move |name, data| {
            let data = match data {
                OutputData::Text(text) => object([("text", encode(&text))]),
                OutputData::Binary(bytes) => encode(&bytes),
            };
            let pending = hub.call(
                "codemode.write",
                object([
                    ("writer", encode(&writer)),
                    ("name", encode(&name)),
                    ("data", data),
                ]),
            );
            Box::pin(async move {
                let reply = pending
                    .await?
                    .ok_or_else(|| Error::message("Missing output-file result"))?;
                Ok(if reply["path"].is_string() {
                    OutputFileResult::Saved(field(&reply, "path")?)
                } else {
                    OutputFileResult::Failed(field(&reply, "error")?)
                })
            })
        });
        let options = FormatResultOptions {
            generated_images: field(options, "generatedImages")?,
            wall_ms: field(options, "wallMs")?,
            max_output_tokens: field(options, "maxOutputTokens")?,
            calls: field(options, "calls")?,
            max_bytes: field(&options["limits"], "maxBytes")?,
            max_lines: field(&options["limits"], "maxLines")?,
            write,
        };
        let pending = format_result(request["result"].clone(), options);
        Ok(Box::pin(async move {
            let result = pending.await?;
            let mut reply = object([("content", encode(&result.content))]);
            if let Some(path) = result.full_output_path {
                reply
                    .as_object_mut()
                    .unwrap()
                    .insert("fullOutputPath".into(), encode(&path));
            }
            Ok(Some(reply))
        }))
    }
}
