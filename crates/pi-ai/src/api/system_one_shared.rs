//! TypeSafe System One protocol over different transports (port of `api/system-one-shared.ts`).

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::classifier_shared::{normalize_classifier_error, parse_classifier_usage, post_classifier_request, required_number};
use crate::types::{
    ClassifierAnswer, ClassifierContext, ClassifierModel, ClassifierQuestion, ClassifierResult, ClassifierStopReason,
    ImageContent, ModelInputModality, ProviderHeaders, StreamOptions, SystemOneImageInput,
};
use crate::utils::error_body::format_provider_error;

/// TypeSafe System One request body without the transport-specific envelope.
///
/// Images take one of three forms (`SystemOneCompat.imageInput`):
/// - `assets`: TypeSafe's `assets` map, `{ "<image-1>": { type: "image", format, data_base64 } }`,
///   with the names placed before the state: `state: ["<image-1>", state]`. PNG and JPEG only.
/// - `images`: Clef's `images` list of data URLs, which the model reads before the state.
///   PNG, JPEG, and WebP, at most four.
/// - `state-image-urls`: OpenRouter's form, data URL parts placed before the state:
///   `state: [{ type: "image_url", image_url: { url } }, state]`. OpenRouter accepts this for every
///   model, and drops `assets` silently.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemOneWireRequest {
    /// The context state, or with images a list of the images or their asset names followed by
    /// the state.
    pub state: Value,
    pub questions: Map<String, Value>,
    /// Images as System One assets, keyed by the names the state references.
    pub assets: Option<Map<String, Value>>,
    /// Images as data URLs, Clef's extension to System One.
    pub images: Option<Vec<String>>,
}

impl SystemOneWireRequest {
    /// `{ state, questions, assets?, images? }` as a JSON object.
    pub fn to_map(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("state".into(), self.state.clone());
        map.insert("questions".into(), Value::Object(self.questions.clone()));
        if let Some(images) = &self.images {
            map.insert("images".into(), json!(images));
        }
        if let Some(assets) = &self.assets {
            map.insert("assets".into(), Value::Object(assets.clone()));
        }
        map
    }

    /// `{ state, questions, assets?, images? }` as JSON.
    pub fn to_value(&self) -> Value {
        Value::Object(self.to_map())
    }
}

/// System One compatibility settings with every field resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSystemOneCompat {
    pub image_input: SystemOneImageInput,
    pub send_image_opt_in_header: bool,
}

/// TypeSafe's opt-in header for its experimental image input.
const IMAGE_OPT_IN_HEADER: (&str, &str) = ("TypeSafe-Danger-Zone-Images", "true");

/// Differences between services that serve System One models.
pub struct SystemOneTransport {
    /// Classifier API implemented by this transport.
    pub api: &'static str,
    /// Service name used in error messages.
    pub label: &'static str,
    /// Compatibility settings for models that do not set them in `compat`.
    pub detect_compat: fn(&ClassifierModel) -> ResolvedSystemOneCompat,
    /// Absolute request URL.
    pub url: fn(&ClassifierModel) -> String,
    /// Wraps the System One request in the service's request envelope.
    pub payload: fn(&ClassifierModel, &SystemOneWireRequest) -> Value,
    /// Extracts the System One output (`{ answers, usage }`) from the response envelope.
    pub output: fn(Value) -> anyhow::Result<Map<String, Value>>,
}

/// `new URL(path, `${baseUrl without trailing slashes}/`)`.
pub fn join_base_url(base_url: &str, path: &str) -> String {
    let base = format!("{}/", base_url.trim_end_matches('/'));
    match url::Url::parse(&base).and_then(|b| b.join(path)) {
        Ok(url) => url.to_string(),
        Err(_) => format!("{base}{path}"),
    }
}

fn probabilities(label: &str, value: Option<&Value>, id: &str) -> anyhow::Result<IndexMap<String, f64>> {
    let Some(map) = value.and_then(Value::as_object) else {
        anyhow::bail!("{label} returned invalid probabilities for {id}");
    };
    map.iter()
        .map(|(key, probability)| Ok((key.clone(), required_number(label, Some(probability), &format!("probability for {id}.{key}"))?)))
        .collect()
}

fn parse_answers(label: &str, value: Option<&Value>, context: &ClassifierContext) -> anyhow::Result<IndexMap<String, ClassifierAnswer>> {
    let Some(value) = value.and_then(Value::as_object) else {
        anyhow::bail!("{label} returned an unexpected response");
    };
    let mut answers = IndexMap::new();
    for (id, question) in &context.questions {
        let Some(answer) = value.get(id).filter(|a| a.is_object()) else {
            anyhow::bail!("{label} did not return an answer for {id}");
        };
        let kind = answer.get("type").and_then(Value::as_str);
        let parsed = match question {
            ClassifierQuestion::Choice { .. } => {
                let (Some("choice"), Some(choice)) = (kind, answer.get("choice").and_then(Value::as_str)) else {
                    anyhow::bail!("{label} did not return a choice answer for {id}");
                };
                ClassifierAnswer::Choice {
                    choice: choice.to_string(),
                    probabilities: probabilities(label, answer.get("probabilities"), id)?,
                    confidence: required_number(label, answer.get("confidence"), &format!("confidence for {id}"))?,
                }
            }
            ClassifierQuestion::Score { .. } => {
                if kind != Some("score") {
                    anyhow::bail!("{label} did not return a score answer for {id}");
                }
                ClassifierAnswer::Score {
                    score: required_number(label, answer.get("score"), &format!("score for {id}"))?,
                    confidence: required_number(label, answer.get("confidence"), &format!("confidence for {id}"))?,
                }
            }
            ClassifierQuestion::Bool { .. } => {
                if kind != Some("noul") {
                    anyhow::bail!("{label} did not return a bool answer for {id}");
                }
                ClassifierAnswer::Bool {
                    probability: required_number(label, answer.get("noul"), &format!("probability for {id}"))?,
                }
            }
        };
        answers.insert(id.clone(), parsed);
    }
    Ok(answers)
}

/// Clef accepts at most this many images per request.
const MAX_DATA_URL_IMAGES: usize = 4;

fn asset_format(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpeg"),
        _ => None,
    }
}

fn data_url(image: &ImageContent) -> String {
    format!("data:{};base64,{}", image.mime_type, image.data)
}

/// Adds the context images to `request` in the service's form.
fn add_images(
    label: &str,
    model: &ClassifierModel,
    input: SystemOneImageInput,
    images: &[ImageContent],
    request: &mut SystemOneWireRequest,
) -> anyhow::Result<()> {
    match input {
        SystemOneImageInput::Images => {
            if images.len() > MAX_DATA_URL_IMAGES {
                anyhow::bail!(
                    "{label} accepts at most {MAX_DATA_URL_IMAGES} images for {}, got {}",
                    model.id,
                    images.len()
                );
            }
            let mut urls = Vec::with_capacity(images.len());
            for image in images {
                if !matches!(image.mime_type.as_str(), "image/png" | "image/jpeg" | "image/webp") {
                    anyhow::bail!("{label} accepts PNG, JPEG, and WebP images for {}, got {}", model.id, image.mime_type);
                }
                urls.push(data_url(image));
            }
            request.images = Some(urls);
        }
        SystemOneImageInput::StateImageUrls => {
            // OpenRouter validates the images against the model that serves the request.
            let mut state: Vec<Value> = images
                .iter()
                .map(|image| json!({ "type": "image_url", "image_url": { "url": data_url(image) } }))
                .collect();
            state.push(std::mem::take(&mut request.state));
            request.state = Value::Array(state);
        }
        SystemOneImageInput::Assets => {
            let mut assets = Map::new();
            for (index, image) in images.iter().enumerate() {
                let Some(format) = asset_format(&image.mime_type) else {
                    anyhow::bail!("{label} accepts PNG and JPEG images for {}, got {}", model.id, image.mime_type);
                };
                assets.insert(
                    format!("<image-{}>", index + 1),
                    json!({ "type": "image", "format": format, "data_base64": image.data }),
                );
            }
            // Every asset must be referenced from the request. The names go before the state, where
            // Clef also places its images.
            let mut state: Vec<Value> = assets.keys().map(|name| Value::String(name.clone())).collect();
            state.push(std::mem::take(&mut request.state));
            request.state = Value::Array(state);
            request.assets = Some(assets);
        }
    }
    Ok(())
}

/// Map public `bool` questions to TypeSafe's wire-level `noul` type.
pub fn wire_request(context: &ClassifierContext) -> SystemOneWireRequest {
    let questions = context
        .questions
        .iter()
        .map(|(id, question)| {
            let mut value = serde_json::to_value(question).unwrap_or(Value::Null);
            if matches!(question, ClassifierQuestion::Bool { .. })
                && let Value::Object(map) = &mut value {
                    map.insert("type".into(), json!("noul"));
                }
            (id.clone(), value)
        })
        .collect();
    SystemOneWireRequest { state: Value::Object(context.state.clone()), questions, assets: None, images: None }
}

/// Run one System One classification over the given transport. Never fails: errors become
/// results.
pub async fn classify_system_one(
    transport: &SystemOneTransport,
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: &StreamOptions,
) -> ClassifierResult {
    let mut output = ClassifierResult {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        answers: IndexMap::new(),
        usage: None,
        stop_reason: ClassifierStopReason::Stop,
        error_message: None,
        timestamp: crate::utils::now_ms(),
    };
    let result: anyhow::Result<()> = async {
        if model.api != transport.api {
            anyhow::bail!("Unsupported classifier API: {}", model.api);
        }
        let mut request = wire_request(context);
        let mut image_options = None;
        if let Some(images) = context.images.as_ref().filter(|i| !i.is_empty()) {
            if !model.input.contains(&ModelInputModality::Image) {
                anyhow::bail!("{} does not support image input for {}", transport.label, model.id);
            }
            let detected = (transport.detect_compat)(model);
            let compat = model.compat.as_deref();
            let image_input = compat.and_then(|c| c.image_input).unwrap_or(detected.image_input);
            add_images(transport.label, model, image_input, images, &mut request)?;
            if compat.and_then(|c| c.send_image_opt_in_header).unwrap_or(detected.send_image_opt_in_header) {
                // `{ ...IMAGE_OPT_IN_HEADERS, ...options.headers }`: caller headers win.
                let mut headers = ProviderHeaders::new();
                headers.insert(IMAGE_OPT_IN_HEADER.0.into(), Some(IMAGE_OPT_IN_HEADER.1.into()));
                for (name, value) in options.headers.iter().flatten() {
                    headers.insert(name.clone(), value.clone());
                }
                image_options = Some(StreamOptions { headers: Some(headers), ..options.clone() });
            }
        }
        let body = post_classifier_request(
            transport.label,
            &(transport.url)(model),
            model,
            (transport.payload)(model, &request),
            image_options.as_ref().unwrap_or(options),
            &[],
        )
        .await?;
        let result = (transport.output)(body)?;
        // Set before parsing answers: a request with malformed answers was still billed.
        if let Some(usage) = parse_classifier_usage(result.get("usage"), model) {
            output.usage = Some(usage);
        }
        output.answers = parse_answers(transport.label, result.get("answers"), context)?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        output.stop_reason =
            if options.is_aborted() { ClassifierStopReason::Aborted } else { ClassifierStopReason::Error };
        output.error_message =
            Some(format_provider_error(&normalize_classifier_error(&error), Some(&format!("{} error", transport.label))));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_base_urls_like_the_url_constructor() {
        assert_eq!(join_base_url("https://api.typesafe.ai/v1/", "systemone"), "https://api.typesafe.ai/v1/systemone");
        assert_eq!(join_base_url("https://openrouter.ai/api/v1", "systemone"), "https://openrouter.ai/api/v1/systemone");
        assert_eq!(join_base_url("https://x.test/a//", "run"), "https://x.test/a/run");
    }
}
