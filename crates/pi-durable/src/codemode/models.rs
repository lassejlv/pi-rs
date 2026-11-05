//! Script-visible model catalogs and durable classifier/image tool effects.
#[cfg(test)]
mod tests;
use crate::{
    harness::{declaration::ToolDeclaration, models::RequestScope, registry::ToolCaller, tool::*},
    storage::StorageFuture,
    *,
};
use pi_ai::{AnyModel, ModelType, StreamOptions, models::Models};
use std::{any::Any, sync::Arc};

pub const MODELS_DESCRIPTION: &str = r#"`models` (all functions return promises):
- `models.getModelsOfType(type, provider?)`, `models.getAvailableOfType(type, provider?)` (configured auth only), `models.getModelOfType(type, provider, id)`: the catalog; `type` is "chat", "image", or "classifier".
- `models.classify(model, { state: { ... }, images?: [{ type: "image", data, mimeType }], questions: { <id>: { type: "choice", instructions, criteria: { <label>: <meaning> } } | { type: "score", instructions, criteria: [<lowest level>, ..., <highest level>] } | { type: "bool", instructions, criteria: { true: <meaning>, false: <meaning> } } } })` resolves to `{ answers: { <id>: ... }, stopReason, errorMessage?, usage? }`.
- `models.generateImages(model, { input: [{ type: "text", text: <prompt> }, ...optional { type: "image", data, mimeType }] })` resolves to `{ output: [image blocks], stopReason, errorMessage?, usage? }`; show each image with `image(block)`.
At most 4 classify/generateImages calls run at once; more wait."#;
pub const CODEMODE_INTERNAL_PREFIX: &str = "pi.codemode.";
pub const CLASSIFY_TOOL: &str = "pi.codemode.models.classify";
pub const GENERATE_IMAGES_TOOL: &str = "pi.codemode.models.generateImages";
pub const MAX_CONCURRENT_MODEL_CALLS: usize = 4;
pub const CATALOG_GLOBALS: &[&str] = &[
    "models.getModelsOfType",
    "models.getAvailableOfType",
    "models.getModelOfType",
];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelEffect {
    Classify,
    GenerateImages,
}
impl ModelEffect {
    pub fn kind(self) -> ModelType {
        match self {
            Self::Classify => ModelType::Classifier,
            Self::GenerateImages => ModelType::Image,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Classify => "models.classify",
            Self::GenerateImages => "models.generateImages",
        }
    }
    pub fn tool(self) -> &'static str {
        match self {
            Self::Classify => CLASSIFY_TOOL,
            Self::GenerateImages => GENERATE_IMAGES_TOOL,
        }
    }
}
#[derive(Clone)]
pub struct CatalogModel {
    pub reference: ModelRef,
    value: Arc<dyn Any + Send + Sync>,
    info: Arc<dyn Fn() -> Result<JsonValue> + Send + Sync>,
}
impl CatalogModel {
    pub fn new<T: Any + Send + Sync>(
        reference: ModelRef,
        value: T,
        info: impl Fn() -> Result<JsonValue> + Send + Sync + 'static,
    ) -> Self {
        Self {
            reference,
            value: Arc::new(value),
            info: Arc::new(info),
        }
    }
    pub fn value<T: Any>(&self) -> Result<&T> {
        self.value
            .downcast_ref()
            .ok_or_else(|| Error::message("Model belongs to a different catalog adapter"))
    }
    pub fn info(&self) -> Result<JsonValue> {
        let mut info = (self.info)()?;
        if let Some(object) = info.as_object_mut() {
            object.shift_remove(&JsonString::from("headers"));
        }
        Ok(info)
    }
}
pub trait CodemodeModels: Send + Sync {
    fn models(&self, kind: ModelType, provider: Option<JsonString>) -> Result<Vec<CatalogModel>>;
    fn available(
        &self,
        kind: ModelType,
        provider: Option<JsonString>,
        context: Context,
    ) -> StorageFuture<Vec<CatalogModel>>;
    fn model(&self, kind: ModelType, reference: &ModelRef) -> Result<Option<CatalogModel>>;
    fn request(
        &self,
        effect: ModelEffect,
        model: CatalogModel,
        input: JsonValue,
        context: Context,
    ) -> StorageFuture<JsonValue>;
}
#[derive(Clone)]
pub struct SdkCodemodeModels(pub Models);
fn sdk_text(text: &JsonString) -> Result<String> {
    text.to_string_checked()
        .map_err(|error| Error::message(error.to_string()))
}
fn sdk_model(model: AnyModel) -> CatalogModel {
    let info = model.clone();
    CatalogModel::new(
        ModelRef {
            provider: model.provider().into(),
            model_id: model.id().into(),
        },
        model,
        move || Ok(wire::encode(&info)),
    )
}
impl CodemodeModels for SdkCodemodeModels {
    fn models(&self, kind: ModelType, provider: Option<JsonString>) -> Result<Vec<CatalogModel>> {
        // SDK providers are UTF8, so an unpaired surrogate filters every model out.
        let Ok(provider) = provider
            .as_ref()
            .map(JsonString::to_string_checked)
            .transpose()
        else {
            return Ok(Vec::new());
        };
        Ok(self
            .0
            .get_models_of_type(kind, provider.as_deref())
            .into_iter()
            .map(sdk_model)
            .collect())
    }
    fn available(
        &self,
        kind: ModelType,
        provider: Option<JsonString>,
        context: Context,
    ) -> StorageFuture<Vec<CatalogModel>> {
        let models = self.0.clone();
        Box::pin(async move {
            let provider = provider.as_ref().map(sdk_text).transpose()?;
            let mut options = StreamOptions::default();
            let signal = context
                .abort_signal()
                .cloned()
                .unwrap_or_else(|| pi_chord::AbortController::new().signal());
            let _scope = RequestScope::new(&mut options, signal, &context);
            Ok(models
                .get_available_of_type(kind, provider.as_deref(), options.signal)
                .await
                .map_err(|error| Error::message(error.to_string()))?
                .into_iter()
                .map(sdk_model)
                .collect())
        })
    }
    fn model(&self, kind: ModelType, reference: &ModelRef) -> Result<Option<CatalogModel>> {
        // As in `getModelOfType`, an identity no SDK model can have finds nothing.
        let (Ok(provider), Ok(id)) = (
            reference.provider.to_string_checked(),
            reference.model_id.to_string_checked(),
        ) else {
            return Ok(None);
        };
        Ok(self
            .0
            .get_model_of_type(kind, &provider, &id)
            .map(sdk_model))
    }
    fn request(
        &self,
        effect: ModelEffect,
        model: CatalogModel,
        input: JsonValue,
        context: Context,
    ) -> StorageFuture<JsonValue> {
        let models = self.0.clone();
        Box::pin(async move {
            let model = model.value::<AnyModel>()?;
            let mut options = StreamOptions::default();
            let signal = context
                .abort_signal()
                .cloned()
                .unwrap_or_else(|| pi_chord::AbortController::new().signal());
            let _scope = RequestScope::new(&mut options, signal, &context);
            match (effect, model) {
                (ModelEffect::Classify, AnyModel::Classifier(model)) => {
                    let checked = wire::decode(&input).map_err(Error::message)?;
                    Ok(wire::encode(
                        &models.classify(model, &checked, options).await,
                    ))
                }
                (ModelEffect::GenerateImages, AnyModel::Image(model)) => {
                    let checked = wire::decode(&input).map_err(Error::message)?;
                    Ok(wire::encode(
                        &models.generate_images(model, &checked, options).await,
                    ))
                }
                _ => Err(Error::message("Catalog returned the wrong model type")),
            }
        })
    }
}
fn json(value: Option<&JsonValue>) -> String {
    value.map_or_else(|| "undefined".into(), JsonValue::to_string)
}
fn describe(value: Option<&JsonValue>) -> JsonString {
    match value {
        None => "undefined".into(),
        Some(JsonValue::Null) => "null".into(),
        Some(JsonValue::Array(values)) => {
            if values.is_empty() {
                "an empty array".into()
            } else {
                "an array".into()
            }
        }
        Some(JsonValue::Object(object)) => {
            if object.is_empty() {
                "{}".into()
            } else {
                join(
                    [
                        "{ ".into(),
                        join(object.keys().take(6).cloned(), ", "),
                        if object.len() > 6 {
                            ", ...".into()
                        } else {
                            JsonString::default()
                        },
                        " }".into(),
                    ],
                    "",
                )
            }
        }
        Some(JsonValue::String(_)) => "a string".into(),
        Some(JsonValue::Bool(_)) => "a boolean".into(),
        _ => "a number".into(),
    }
}
fn message(parts: impl IntoIterator<Item = JsonString>) -> Error {
    Error::host("Error", join(parts, ""))
}
fn got(prefix: impl Into<JsonString>, value: Option<&JsonValue>) -> JsonString {
    join([prefix.into(), describe(value)], "")
}
fn kind(value: Option<&JsonValue>) -> Result<ModelType> {
    match value.and_then(JsonValue::to_utf8).as_deref() {
        Some("chat") => Ok(ModelType::Chat),
        Some("image") => Ok(ModelType::Image),
        Some("classifier") => Ok(ModelType::Classifier),
        _ => Err(Error::message(format!(
            "Unknown model type {}. Use \"chat\", \"image\", or \"classifier\".",
            json(value)
        ))),
    }
}
fn provider(value: Option<&JsonValue>) -> Result<Option<JsonString>> {
    match value {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(value)) => Ok(Some(value.clone())),
        _ => Err(Error::message("provider must be a string")),
    }
}
pub fn catalog_call(
    name: &str,
    args: Option<JsonValue>,
    models: Arc<dyn CodemodeModels>,
    context: Context,
) -> StorageFuture<Option<JsonValue>> {
    let name = name.to_owned();
    Box::pin(async move {
        let args = args
            .as_ref()
            .and_then(JsonValue::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if name == CATALOG_GLOBALS[0] || name == CATALOG_GLOBALS[1] {
            let kind = kind(args.first())?;
            let provider = provider(args.get(1))?;
            let models = if name == CATALOG_GLOBALS[0] {
                models.models(kind, provider)?
            } else {
                models.available(kind, provider, context).await?
            };
            return Ok(Some(JsonValue::Array(
                models
                    .iter()
                    .map(CatalogModel::info)
                    .collect::<Result<Vec<_>>>()?
                    .into(),
            )));
        }
        let (Some(provider), Some(id)) = (
            args.get(1).and_then(JsonValue::as_string),
            args.get(2).and_then(JsonValue::as_string),
        ) else {
            return Err(message([
                "models.getModelOfType(type, provider, id) expects three strings, got (".into(),
                join(args.iter().map(|value|describe(Some(value))),", "),
                "). The provider and the id are separate arguments, for example models.getModelOfType(\"classifier\", \"typesafe\", \"jev-latest\").".into(),
            ]));
        };
        models
            .model(
                kind(args.first())?,
                &ModelRef {
                    provider: provider.clone(),
                    model_id: id.clone(),
                },
            )?
            .map(|model| model.info())
            .transpose()
    })
}
fn article(kind: ModelType) -> &'static str {
    match kind {
        ModelType::Chat => "a chat",
        ModelType::Image => "an image",
        ModelType::Classifier => "a classifier",
    }
}
fn resolve_model(
    effect: ModelEffect,
    value: Option<&JsonValue>,
    models: &dyn CodemodeModels,
) -> Result<CatalogModel> {
    let kind = effect.kind();
    let hint = format!(
        "List the {} models you can use with models.getAvailableOfType(\"{}\").",
        kind.as_str(),
        kind.as_str()
    );
    let provider = value.and_then(|v| v["provider"].as_string());
    let id = value.and_then(|v| v["id"].as_string());
    let (Some(provider), Some(id)) = (provider, id) else {
        return Err(message([
            format!(
                "{}() expects {} model as its first argument, got ",
                effect.label(),
                article(kind)
            )
            .into(),
            describe(value),
            ".".into(),
            if value.is_none_or(JsonValue::is_null) {
                " models.getModelOfType() returns undefined for an unknown provider or id.".into()
            } else {
                JsonString::default()
            },
            " ".into(),
            hint.into(),
        ]));
    };
    let reference = ModelRef {
        provider: provider.clone(),
        model_id: id.clone(),
    };
    if let Some(model) = models.model(kind, &reference)? {
        return Ok(model);
    }
    let named = join([provider.clone(), id.clone()], "/");
    for other in [ModelType::Chat, ModelType::Image, ModelType::Classifier] {
        if other != kind && models.model(other, &reference)?.is_some() {
            return Err(message([
                "\"".into(),
                named,
                format!(
                    "\" is {} model, not {} model. {hint}",
                    article(other),
                    article(kind)
                )
                .into(),
            ]));
        }
    }
    Err(message([
        format!("Unknown {} model \"", kind.as_str()).into(),
        named,
        format!("\". {hint}").into(),
    ]))
}
const CLASSIFIER_SHAPE: &str = "{ state: { ... }, images?: [{ type: \"image\", data: <base64>, mimeType }], questions: { <id>: { type: \"choice\", instructions, criteria: { <label>: <meaning> } } | { type: \"score\", instructions, criteria: [<lowest level>, ..., <highest level>] } | { type: \"bool\", instructions, criteria: { true: <meaning>, false: <meaning> } } } }";
fn is_image(value: &JsonValue) -> bool {
    value.is_object()
        && value["type"].string_equals("image")
        && value["data"].is_string()
        && value["mimeType"].is_string()
}
pub fn check_context(effect: ModelEffect, value: Option<&JsonValue>) -> Result<JsonValue> {
    let fail = |problem: JsonString| {
        message([
            format!("{}() ", effect.label()).into(),
            problem,
            ". Expected context: ".into(),
            if effect == ModelEffect::Classify {
                CLASSIFIER_SHAPE.into()
            } else {
                "{ input: [{ type: \"text\", text: <prompt> }, ...optional { type: \"image\", data: <base64>, mimeType } references] }".into()
            },
            ".".into(),
        ])
    };
    let Some(context) = value.filter(|value| value.is_object()) else {
        return Err(fail(got(
            "expects a context object as its second argument, got ",
            value,
        )));
    };
    if effect == ModelEffect::GenerateImages {
        let input = wire::get(context, "input");
        let Some(blocks) = input
            .and_then(JsonValue::as_array)
            .filter(|blocks| !blocks.is_empty())
        else {
            return Err(fail(got(
                "context.input must be a non-empty array of blocks, got ",
                input,
            )));
        };
        for (index, block) in blocks.iter().enumerate() {
            if is_image(block)
                || block.is_object()
                    && block["type"].string_equals("text")
                    && block["text"].is_string()
            {
                continue;
            }
            return Err(fail(got(
                format!("context.input[{index}] must be a text or image block, got "),
                Some(block),
            )));
        }
        return Ok(context.clone());
    }
    let state = wire::get(context, "state");
    if !state.is_some_and(JsonValue::is_object) {
        return Err(fail(got("context.state must be an object, got ", state)));
    }
    if let Some(images) = wire::get(context, "images") {
        let Some(images) = images.as_array() else {
            return Err(fail(got(
                "context.images must be an array, got ",
                Some(images),
            )));
        };
        for (index, image) in images.iter().enumerate() {
            if !is_image(image) {
                return Err(fail(got(
                    format!("context.images[{index}] must be an image block, got "),
                    Some(image),
                )));
            }
        }
    }
    let questions = wire::get(context, "questions");
    let Some(questions) = questions
        .and_then(JsonValue::as_object)
        .filter(|questions| !questions.is_empty())
    else {
        return Err(fail(got(
            "context.questions must map question IDs to questions, got ",
            questions,
        )));
    };
    for (id, question) in questions {
        let at = join(["context.questions.".into(), id.clone()], "");
        let suffix = |tail: &str| join([at.clone(), tail.into()], "");
        if !question.is_object() {
            return Err(fail(got(
                suffix(" must be a question object, got "),
                Some(question),
            )));
        }
        if !question["instructions"].is_string() {
            return Err(fail(suffix(".instructions must be a string")));
        }
        let criteria = &question["criteria"];
        match question["type"].to_utf8().as_deref() {
            Some("choice") => {
                if !criteria.as_object().is_some_and(|values| {
                    !values.is_empty() && values.values().all(JsonValue::is_string)
                }) {
                    return Err(fail(suffix(
                        " is a \"choice\" question, so criteria must map each label to its meaning",
                    )));
                }
            }
            Some("score") => {
                if !criteria.as_array().is_some_and(|values| {
                    !values.is_empty() && values.iter().all(JsonValue::is_string)
                }) {
                    return Err(fail(suffix(
                        " is a \"score\" question, so criteria must list the levels as strings, lowest first",
                    )));
                }
            }
            Some("bool") => {
                if !criteria.is_object()
                    || !criteria["true"].is_string()
                    || !criteria["false"].is_string()
                {
                    return Err(fail(suffix(
                        " is a \"bool\" question, so criteria must be { true: string, false: string }",
                    )));
                }
            }
            _ => {
                return Err(fail(join(
                    [
                        suffix(".type must be \"choice\", \"score\", or \"bool\", got "),
                        json(wire::get(question, "type")).into(),
                    ],
                    "",
                )));
            }
        }
    }
    Ok(context.clone())
}
pub fn model_result(
    effect: ModelEffect,
    model: &CatalogModel,
    result: JsonValue,
) -> Result<ToolExecutionResult> {
    let ok = result["stopReason"].string_equals("stop");
    let label = join(
        [
            if effect == ModelEffect::Classify {
                "classify ".into()
            } else {
                "generateImages ".into()
            },
            model.reference.provider.clone(),
            "/".into(),
            model.reference.model_id.clone(),
        ],
        "",
    );
    let text = if ok {
        join([label, " done".into()], "")
    } else {
        join(
            [
                label,
                " ".into(),
                wire::field(&result, "stopReason").map_err(Error::message)?,
                ": ".into(),
                result["errorMessage"]
                    .as_string()
                    .cloned()
                    .unwrap_or_default(),
            ],
            "",
        )
    };
    Ok(ToolExecutionResult {
        output: Some(Arc::new(vec![text_content(text)])),
        structured_output: Some(result.clone().into()),
        is_error: (!ok).then_some(true),
        usage: wire::optional(&result, "usage").map_err(Error::message)?,
        ..Default::default()
    })
}
pub type ModelsServiceReader =
    Arc<dyn Fn(&ToolExecutionApi) -> Result<Arc<dyn CodemodeModels>> + Send + Sync>;
pub fn create_model_tools() -> Vec<NativeTool> {
    create_model_tools_with(Arc::new(|api| {
        api.model_collection()?
            .codemode_models()
            .ok_or_else(|| Error::message("Model collection does not expose typed models"))
    }))
}
pub fn create_model_tools_with(read: ModelsServiceReader) -> Vec<NativeTool> {
    [ModelEffect::Classify,ModelEffect::GenerateImages].into_iter().map(|effect| {
        let read=read.clone();
        let schema=wire::encode(&serde_json::json!({"type":"object","properties":{"model":{},"context":{}},"required":["model","context"]}));
        let mut tool=NativeTool::new(ToolDeclaration::new(effect.tool(),format!("{}() of code mode scripts.",effect.label()),schema),Arc::new(move |args,api,context| {
            let read=read.clone();Box::pin(async move {
                let models=read(&api)?;
                let model=resolve_model(effect,wire::get(args.value(),"model"),models.as_ref())?;
                let checked=check_context(effect,wire::get(args.value(),"context"))?;
                let result=read(&api)?.request(effect,model.clone(),checked,context).await?;
                model_result(effect,&model,result)
            })
        }));
        tool.callers=Some(vec![ToolCaller::Tools]);tool.structured_output_schema=Some(wire::object());tool
    }).collect()
}
