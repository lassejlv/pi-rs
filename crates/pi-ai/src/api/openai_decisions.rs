//! OpenAI's Decisions API (port of `api/openai-decisions.ts`).
//!
//! `POST /v1/decisions` with `{ model, input, questions }`. The state is sent as JSON text. With
//! images, the input becomes one user message with the state as `input_text` followed by
//! `input_image` data URLs. Questions map to Decisions types: `choice` to `choice`, `score` to
//! `score`, and `bool` to `predicate`. Predicates have no criteria field, so the meanings of
//! true and false are appended to the instructions.
//!
//! Only OpenAI API keys work: Sign in with ChatGPT tokens are rejected on this route.

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::classifier_shared::{
    is_record, normalize_classifier_error, parse_classifier_usage, post_classifier_request,
    required_number,
};
use crate::models::ProviderClassifier;
use crate::types::{
    ClassifierAnswer, ClassifierContext, ClassifierModel, ClassifierQuestion, ClassifierResult,
    ClassifierStopReason, StreamOptions,
};
use crate::utils::error_body::{ProviderHttpError, format_provider_error};

const LABEL: &str = "OpenAI Decisions";
/// The endpoint accepts at most this many image parts per request.
const MAX_IMAGES: usize = 128;
/// Cloudflare in front of api.openai.com answers 504 with an HTML page when a request runs longer
/// than about five seconds; retrying the same input runs into it again, so 504 is not retried.
const NO_RETRY_STATUSES: [u16; 1] = [504];

fn predicate_instructions(instructions: &str, when_true: &str, when_false: &str) -> String {
    let meanings: Vec<String> = [
        (!when_true.is_empty()).then(|| format!("True means: {when_true}")),
        (!when_false.is_empty()).then(|| format!("False means: {when_false}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if meanings.is_empty() {
        instructions.to_string()
    } else {
        format!("{instructions}\n\n{}", meanings.join("\n"))
    }
}

fn wire_question(name: &str, question: &ClassifierQuestion) -> Value {
    match question {
        ClassifierQuestion::Choice {
            instructions,
            criteria,
        } => json!({
            "type": "choice",
            "name": name,
            "instructions": instructions,
            "choices": criteria
                .iter()
                .map(|(value, description)| {
                    if description.is_empty() { json!({ "value": value }) } else { json!({ "value": value, "description": description }) }
                })
                .collect::<Vec<_>>(),
        }),
        ClassifierQuestion::Score {
            instructions,
            criteria,
        } => json!({
            "type": "score",
            "name": name,
            "instructions": instructions,
            "levels": criteria.iter().map(|label| json!({ "label": label })).collect::<Vec<_>>(),
        }),
        ClassifierQuestion::Bool {
            instructions,
            criteria,
        } => json!({
            "type": "predicate",
            "name": name,
            "instructions": predicate_instructions(instructions, &criteria.true_, &criteria.false_),
        }),
    }
}

fn wire_input(context: &ClassifierContext) -> anyhow::Result<Value> {
    let state = Value::Object(context.state.clone()).to_string();
    let images = context.images.as_deref().unwrap_or_default();
    if images.is_empty() {
        return Ok(Value::String(state));
    }
    if images.len() > MAX_IMAGES {
        anyhow::bail!(
            "{LABEL} accepts at most {MAX_IMAGES} images, got {}",
            images.len()
        );
    }
    let mut content = vec![json!({ "type": "input_text", "text": state })];
    for image in images {
        content.push(json!({ "type": "input_image", "image_url": format!("data:{};base64,{}", image.mime_type, image.data) }));
    }
    Ok(json!([{ "role": "user", "content": content }]))
}

fn choice_probabilities(value: Option<&Value>, id: &str) -> anyhow::Result<IndexMap<String, f64>> {
    let Some(Value::Array(entries)) = value else {
        anyhow::bail!("{LABEL} returned invalid probabilities for {id}");
    };
    let mut probabilities = IndexMap::new();
    for entry in entries {
        let Some(choice) = entry
            .get("value")
            .and_then(Value::as_str)
            .filter(|_| is_record(entry))
        else {
            anyhow::bail!("{LABEL} returned invalid probabilities for {id}");
        };
        let probability = required_number(
            LABEL,
            entry.get("probability"),
            &format!("probability for {id}.{choice}"),
        )?;
        probabilities.insert(choice.to_string(), probability);
    }
    Ok(probabilities)
}

fn parse_answer(
    id: &str,
    question: &ClassifierQuestion,
    answer: &Map<String, Value>,
) -> anyhow::Result<ClassifierAnswer> {
    let answer_type = answer.get("type").and_then(Value::as_str);
    if answer_type == Some("refusal") {
        anyhow::bail!("{LABEL} refused to answer {id}");
    }
    match question {
        ClassifierQuestion::Choice { .. } => {
            let Some(choice) = answer
                .get("choice")
                .and_then(Value::as_str)
                .filter(|_| answer_type == Some("choice"))
            else {
                anyhow::bail!("{LABEL} did not return a choice answer for {id}");
            };
            Ok(ClassifierAnswer::Choice {
                choice: choice.to_string(),
                probabilities: choice_probabilities(answer.get("probabilities"), id)?,
                confidence: required_number(
                    LABEL,
                    answer.get("confidence"),
                    &format!("confidence for {id}"),
                )?,
            })
        }
        ClassifierQuestion::Score { .. } => {
            if answer_type != Some("score") {
                anyhow::bail!("{LABEL} did not return a score answer for {id}");
            }
            Ok(ClassifierAnswer::Score {
                score: required_number(LABEL, answer.get("score"), &format!("score for {id}"))?,
                confidence: required_number(
                    LABEL,
                    answer.get("confidence"),
                    &format!("confidence for {id}"),
                )?,
            })
        }
        ClassifierQuestion::Bool { .. } => {
            if answer_type != Some("predicate") {
                anyhow::bail!("{LABEL} did not return a predicate answer for {id}");
            }
            Ok(ClassifierAnswer::Bool {
                probability: required_number(
                    LABEL,
                    answer.get("probability"),
                    &format!("probability for {id}"),
                )?,
            })
        }
    }
}

fn parse_answers(
    value: Option<&Value>,
    context: &ClassifierContext,
) -> anyhow::Result<IndexMap<String, ClassifierAnswer>> {
    let Some(Value::Array(answers)) = value else {
        anyhow::bail!("{LABEL} returned an unexpected response");
    };
    let mut by_name: IndexMap<&str, &Map<String, Value>> = IndexMap::new();
    for answer in answers {
        if let (Some(map), Some(name)) = (
            answer.as_object(),
            answer.get("name").and_then(Value::as_str),
        ) {
            by_name.insert(name, map);
        }
    }
    let mut parsed = IndexMap::new();
    for (id, question) in &context.questions {
        let Some(answer) = by_name.get(id.as_str()) else {
            anyhow::bail!("{LABEL} did not return an answer for {id}");
        };
        parsed.insert(id.clone(), parse_answer(id, question, answer)?);
    }
    Ok(parsed)
}

fn error_message(error: &anyhow::Error) -> String {
    if error
        .downcast_ref::<ProviderHttpError>()
        .is_some_and(|e| e.status == Some(504))
    {
        return format!(
            "{LABEL} error (504): the request timed out at the gateway. Very large inputs (above roughly 600K tokens) currently exceed its time limit."
        );
    }
    format_provider_error(
        &normalize_classifier_error(error),
        Some(&format!("{LABEL} error")),
    )
}

/// Classification through OpenAI's Decisions API.
pub async fn classify(
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: StreamOptions,
) -> ClassifierResult {
    let mut output = ClassifierResult {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        answers: IndexMap::new(),
        stop_reason: ClassifierStopReason::Stop,
        timestamp: crate::utils::now_ms(),
        ..Default::default()
    };
    let result: anyhow::Result<()> = async {
        if model.api != "openai-decisions" {
            anyhow::bail!("Unsupported classifier API: {}", model.api);
        }
        let url = format!("{}/decisions", model.base_url.trim_end_matches('/'));
        let questions: Vec<Value> = context
            .questions
            .iter()
            .map(|(id, question)| wire_question(id, question))
            .collect();
        let body =
            json!({ "model": model.id, "input": wire_input(context)?, "questions": questions });
        let response =
            post_classifier_request(LABEL, &url, model, body, &options, &NO_RETRY_STATUSES).await?;
        if !is_record(&response) {
            anyhow::bail!("{LABEL} returned an unexpected response");
        }
        // Set before parsing answers: a request with malformed or refused answers was still billed.
        if let Some(usage) = parse_classifier_usage(response.get("usage"), model) {
            output.usage = Some(usage);
        }
        output.answers = parse_answers(response.get("answers"), context)?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        output.answers = IndexMap::new();
        output.stop_reason = if options.signal.as_ref().is_some_and(|s| s.is_cancelled()) {
            ClassifierStopReason::Aborted
        } else {
            ClassifierStopReason::Error
        };
        output.error_message = Some(error_message(&error));
    }
    output
}

/// The `openai-decisions` classifier API implementation.
pub struct OpenAIDecisionsApi;

#[async_trait]
impl ProviderClassifier for OpenAIDecisionsApi {
    async fn classify(
        &self,
        model: &ClassifierModel,
        context: &ClassifierContext,
        options: StreamOptions,
    ) -> ClassifierResult {
        crate::utils::wasm::assert_send(classify(model, context, options)).await
    }
}
