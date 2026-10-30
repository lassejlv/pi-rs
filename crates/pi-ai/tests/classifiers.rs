//! Offline tests for the classifier APIs (ports of `typesafe-system-one.test.ts`,
//! `cloudflare-workers-ai-system-one.test.ts`, `llama-cpp-classify.test.ts` and the System One
//! routing part of `classifier-models.test.ts`).

mod mock_http;

use std::sync::Arc;

use indexmap::IndexMap;
use mock_http::{MockServer, Request, Response};
use parking_lot::Mutex;
use pi_ai::api::llama_cpp_classify::{self, answer_from_probabilities, label_probabilities, llama_server_root, peak_confidence, render_question};
use pi_ai::api::typesafe_system_one;
use pi_ai::providers::all::{builtin_models, get_builtin_classifier_model, get_builtin_classifier_models};
use pi_ai::providers::cloudflare::cloudflare_workers_ai_provider;
use pi_ai::*;
use serde_json::{Value, json};

fn typesafe_model(base_url: &str) -> ClassifierModel {
    serde_json::from_value(json!({
        "type": "classifier", "id": "jev-latest", "name": "Jev", "api": "typesafe-system-one", "provider": "typesafe",
        "baseUrl": base_url, "input": ["text"], "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }, "contextWindow": 64000
    }))
    .unwrap()
}

fn typesafe_context() -> ClassifierContext {
    serde_json::from_value(json!({
        "state": { "text": "The deployment succeeded, thank you." },
        "questions": {
            "category": { "type": "choice", "instructions": "Classify the message", "criteria": { "success": "Successful", "failure": "Failed" } },
            "satisfaction": { "type": "score", "instructions": "Score satisfaction", "criteria": ["low", "neutral", "high"] },
            "approved": { "type": "bool", "instructions": "Does the user approve?", "criteria": { "true": "Approval", "false": "No approval" } }
        }
    }))
    .unwrap()
}

fn wire_answers() -> Value {
    json!({
        "category": { "type": "choice", "choice": "success", "probabilities": { "success": 0.9, "failure": 0.1 }, "confidence": 0.8 },
        "satisfaction": { "type": "score", "score": 2, "confidence": 0.7 },
        "approved": { "type": "noul", "noul": 0.95 }
    })
}

fn key(key: &str) -> StreamOptions {
    StreamOptions { api_key: Some(key.into()), ..Default::default() }
}

#[tokio::test]
async fn typesafe_maps_bool_questions_to_noul() {
    let server = MockServer::start(|request, _| {
        let payload = request.json();
        if payload["model"] == "priced" {
            return Response::json(200, json!({ "answers": wire_answers(), "usage": { "input_tokens": 308, "output_tokens": 23 } }));
        }
        Response::json(200, json!({ "answers": wire_answers() }))
    })
    .await;
    let model = typesafe_model(&format!("{}/v1/", server.url));
    let options = key("secret");
    let mut with_temperature = options.clone();
    with_temperature.temperature = Some(1.5);
    let result = typesafe_system_one::classify(&model, &typesafe_context(), with_temperature).await;
    let request = server.request(0);
    let payload = request.json();
    assert_eq!(request.path, "/v1/systemone");
    assert_eq!(payload["model"], "jev-latest");
    assert_eq!(payload["questions"]["category"]["type"], "choice");
    assert_eq!(payload["questions"]["satisfaction"]["type"], "score");
    assert_eq!(payload["questions"]["approved"]["type"], "noul");
    assert!(payload.get("temperature").is_none());
    assert_eq!(request.header("authorization"), Some("Bearer secret"));
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop, "{:?}", result.error_message);
    assert_eq!(result.answers["approved"], ClassifierAnswer::Bool { probability: 0.95 });
    assert!(matches!(&result.answers["category"], ClassifierAnswer::Choice { choice, .. } if choice == "success"));
    assert_eq!(result.answers["satisfaction"], ClassifierAnswer::Score { score: 2.0, confidence: 0.7 });
    assert!(result.usage.is_none());

    let mut priced = model.clone();
    priced.id = "priced".into();
    priced.cost.input = 0.042;
    let result = typesafe_system_one::classify(&priced, &typesafe_context(), key("secret")).await;
    let usage = result.usage.unwrap();
    assert_eq!((usage.input, usage.output, usage.total_tokens), (308, 23, 331));
    assert!((usage.cost.total - 0.000012936).abs() < 1e-12);
}

#[tokio::test]
async fn openrouter_serves_system_one() {
    let server = MockServer::start(|request, _| {
        assert_eq!(request.json()["model"], "typesafe/jev-1.13");
        Response::json(
            200,
            json!({ "id": "gen-dec-1", "provider": "TypeSafe", "answers": wire_answers(), "usage": { "input_tokens": 308, "output_tokens": 23, "cost": 0.000012936 } }),
        )
    })
    .await;
    let mut model = typesafe_model(&format!("{}/api/v1", server.url));
    model.id = "typesafe/jev-1.13".into();
    model.provider = "openrouter".into();
    model.cost.input = 0.042;
    let result = typesafe_system_one::classify(&model, &typesafe_context(), key("secret")).await;
    assert_eq!(server.request(0).path, "/api/v1/systemone");
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    assert!((result.usage.unwrap().cost.total - 0.000012936).abs() < 1e-12);

    let models = builtin_models(CreateModelsOptions::default());
    for classifier in get_builtin_classifier_models("openrouter") {
        assert_eq!(classifier.api, "typesafe-system-one");
        assert_eq!(classifier.base_url, "https://openrouter.ai/api/v1");
        assert!(models.get_model("openrouter", &classifier.id).is_none());
        assert_eq!(models.get_model_of_type(ModelType::Classifier, "openrouter", &classifier.id), Some(AnyModel::Classifier(classifier.clone())));
    }
}

#[tokio::test]
async fn typesafe_rejects_other_apis_images_and_bad_responses() {
    let server = MockServer::start(|_, index| match index {
        0 => Response::json(200, json!({ "answers": {}, "usage": { "input_tokens": 10, "output_tokens": 2 } })),
        1 => Response::json(200, json!({ "answers": wire_answers(), "usage": { "input_tokens": "many", "output_tokens": 3 } })),
        _ => Response::json(200, json!({ "answers": wire_answers(), "usage": { "cost": 0.1 } })),
    })
    .await;
    let model = typesafe_model(&server.url);
    let mut other = model.clone();
    other.api = "cloudflare-workers-ai-system-one".into();
    let result = typesafe_system_one::classify(&other, &typesafe_context(), key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert!(result.error_message.unwrap().contains("Unsupported classifier API: cloudflare-workers-ai-system-one"));

    let mut with_images = typesafe_context();
    with_images.images = Some(vec![ImageContent { data: "aW1hZ2U=".into(), mime_type: "image/png".into() }]);
    let result = typesafe_system_one::classify(&model, &with_images, key("secret")).await;
    assert_eq!(result.error_message.as_deref(), Some("System One API does not support image input"));
    assert!(server.requests().is_empty());

    let result = typesafe_system_one::classify(&model, &typesafe_context(), key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert!(result.answers.is_empty());
    assert!(result.error_message.unwrap().contains("did not return an answer for category"));
    let usage = result.usage.unwrap();
    assert_eq!((usage.input, usage.output), (10, 2));

    let result = typesafe_system_one::classify(&model, &typesafe_context(), key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    let usage = result.usage.unwrap();
    assert_eq!((usage.input, usage.output, usage.total_tokens), (0, 3, 3));
    let result = typesafe_system_one::classify(&model, &typesafe_context(), key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    assert!(result.usage.is_none());
}

#[tokio::test]
async fn typesafe_merges_headers_and_preserves_question_ids() {
    let server = MockServer::start(|request, _| {
        if request.json()["questions"].get("__proto__").is_some() {
            return Response::json(200, json!({ "answers": { "__proto__": { "type": "noul", "noul": 0.75 } } }));
        }
        Response::json(200, json!({ "answers": wire_answers() }))
    })
    .await;
    let mut model = typesafe_model(&server.url);
    model.headers = Some([("authorization".to_string(), "Bearer model".to_string()), ("X-Source".to_string(), "model".to_string())].into_iter().collect());
    let mut headers = ProviderHeaders::new();
    headers.insert("Authorization".into(), Some("Bearer request".into()));
    headers.insert("x-source".into(), Some("request".into()));
    typesafe_system_one::classify(&model, &typesafe_context(), StreamOptions { headers: Some(headers), ..key("secret") }).await;
    let mut headers = ProviderHeaders::new();
    headers.insert("Authorization".into(), None);
    typesafe_system_one::classify(&model, &typesafe_context(), StreamOptions { headers: Some(headers), ..key("secret") }).await;
    let first = server.request(0);
    assert_eq!(first.header("authorization"), Some("Bearer request"));
    assert_eq!(first.header("x-source"), Some("request"));
    assert_eq!(first.header_count("authorization"), 1);
    assert!(server.request(1).header("authorization").is_none());

    let context: ClassifierContext = serde_json::from_value(json!({
        "state": {}, "questions": { "__proto__": { "type": "bool", "instructions": "Is this true?", "criteria": { "true": "Yes", "false": "No" } } }
    }))
    .unwrap();
    let result = typesafe_system_one::classify(&typesafe_model(&server.url), &context, key("secret")).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(result.answers.keys().collect::<Vec<_>>(), ["__proto__"]);
    assert_eq!(serde_json::to_value(&result.answers).unwrap(), json!({ "__proto__": { "type": "bool", "probability": 0.75 } }));
}

#[tokio::test]
async fn typesafe_timeouts_and_fresh_retry_attempts() {
    let server = MockServer::start(|_, index| match index {
        0 => Response::json(200, json!({ "answers": wire_answers() })).with_delay(500),
        1 => Response::text(500, "retry").with_header("retry-after-ms", "0"),
        _ => Response::json(200, json!({ "answers": wire_answers() })).with_delay(30),
    })
    .await;
    let model = typesafe_model(&server.url);
    let options = StreamOptions { timeout_ms: Some(5), max_retries: Some(0), ..key("secret") };
    let result = typesafe_system_one::classify(&model, &typesafe_context(), options).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert_eq!(result.error_message.as_deref(), Some("Request timed out after 5ms"));

    // Each attempt gets its own timeout: the retry may take most of the timeout again.
    let options = StreamOptions { timeout_ms: Some(1000), max_retries: Some(1), ..key("secret") };
    let result = typesafe_system_one::classify(&model, &typesafe_context(), options).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop, "{:?}", result.error_message);
    assert_eq!(server.requests().len(), 3);
}

// ---------------------------------------------------------------------------
// Cloudflare Workers AI System One
// ---------------------------------------------------------------------------

fn cloudflare_context() -> ClassifierContext {
    serde_json::from_value(json!({
        "state": { "message": "Help! My payouts have been failing for 3 days." },
        "questions": {
            "is_urgent": { "type": "bool", "instructions": "Does this convey urgency?", "criteria": { "true": "Explicitly time-sensitive", "false": "No urgency expressed" } },
            "department": { "type": "choice", "instructions": "Which team should handle this?", "criteria": { "billing": "Payments", "technical": "Bugs" } }
        }
    }))
    .unwrap()
}

fn jev_output() -> Value {
    json!({
        "model": "jev-1.13.0",
        "answers": {
            "is_urgent": { "type": "noul", "noul": 0.95 },
            "department": { "type": "choice", "choice": "billing", "confidence": 0.8, "probabilities": { "billing": 0.87, "technical": 0.13 } }
        },
        "usage": { "input_tokens": 426, "output_tokens": 73 }
    })
}

fn clef_output() -> Value {
    json!({
        "model": "clef",
        "answers": {
            "is_urgent": { "type": "noul", "noul": 0.9912 },
            "department": { "type": "choice", "choice": "technical", "probabilities": { "billing": 0.1632, "technical": 0.8368 }, "confidence": 0.4538 }
        },
        "usage": { "input_tokens": 222, "output_tokens": 0 }
    })
}

fn cloudflare_auth() -> StreamOptions {
    let mut env = ProviderEnv::new();
    env.insert("CLOUDFLARE_ACCOUNT_ID".into(), "account-id".into());
    StreamOptions { api_key: Some("cf-key".into()), env: Some(env), ..Default::default() }
}

#[tokio::test]
async fn cloudflare_system_one_through_models() {
    let server = MockServer::start(|request, _| {
        let payload = request.json();
        let model = payload["model"].as_str().unwrap_or_default().to_string();
        assert_eq!(payload["input"]["questions"]["is_urgent"]["type"], "noul");
        assert_eq!(request.header("authorization"), Some("Bearer cf-key"));
        match model.as_str() {
            "typesafe/jev" => {
                assert_eq!(payload["input"]["questions"]["department"]["type"], "choice");
                Response::json(200, json!({ "result": { "state": "Completed", "result": jev_output(), "gatewayMetadata": { "keySource": "Unified" } }, "success": true, "errors": [], "messages": [] }))
            }
            "queued" => Response::json(200, json!({ "result": { "state": "Queued", "result": null }, "success": true, "errors": [], "messages": [] })),
            "missing" => Response::json(200, json!({ "success": false, "errors": [{ "code": 5007, "message": "No such model" }], "result": null })),
            _ => Response::json(200, json!({ "result": clef_output(), "success": true, "errors": [], "messages": [] })),
        }
    })
    .await;
    let models = create_models(CreateModelsOptions::default());
    models.set_provider(cloudflare_workers_ai_provider());
    let Some(AnyModel::Classifier(jev)) = models.get_model_of_type(ModelType::Classifier, "cloudflare-workers-ai", "typesafe/jev") else {
        panic!("missing Cloudflare Jev model");
    };
    assert_eq!(Some(jev.clone()), get_builtin_classifier_model("cloudflare-workers-ai", "typesafe/jev"));
    assert_eq!(jev.api, "cloudflare-workers-ai-system-one");
    assert!(models.get_model("cloudflare-workers-ai", "typesafe/jev").is_none());

    let local = |model: &ClassifierModel| ClassifierModel {
        base_url: model.base_url.replace("https://api.cloudflare.com", &server.url),
        ..model.clone()
    };
    let result = models.classify(&local(&jev), &cloudflare_context(), cloudflare_auth()).await;
    assert_eq!(server.request(0).path, "/client/v4/accounts/account-id/ai/run");
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop, "{:?}", result.error_message);
    assert_eq!(result.answers["is_urgent"], ClassifierAnswer::Bool { probability: 0.95 });
    assert!(matches!(&result.answers["department"], ClassifierAnswer::Choice { choice, confidence, .. } if choice == "billing" && *confidence == 0.8));
    let usage = result.usage.unwrap();
    assert_eq!((usage.input, usage.output, usage.total_tokens), (426, 73, 499));

    for (id, price) in [("@cf/cloudflare/clef", 0.24), ("@cf/cloudflare/clef-flash", 0.038), ("@cf/cloudflare/clef-omni", 0.15)] {
        let Some(AnyModel::Classifier(clef)) = models.get_model_of_type(ModelType::Classifier, "cloudflare-workers-ai", id) else {
            panic!("missing {id}");
        };
        let result = models.classify(&local(&clef), &cloudflare_context(), cloudflare_auth()).await;
        assert_eq!(result.stop_reason, ClassifierStopReason::Stop, "{:?}", result.error_message);
        assert_eq!(result.answers["is_urgent"], ClassifierAnswer::Bool { probability: 0.9912 });
        let usage = result.usage.unwrap();
        assert_eq!((usage.input, usage.output, usage.total_tokens), (222, 0, 222));
        assert!((usage.cost.input - 222.0 * price / 1_000_000.0).abs() < 1e-12);
    }

    let mut queued = local(&jev);
    queued.id = "queued".into();
    let result = models.classify(&queued, &cloudflare_context(), cloudflare_auth()).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert!(result.error_message.unwrap().contains("run did not complete (state: Queued)"));

    let mut missing = local(&jev);
    missing.id = "missing".into();
    let result = models.classify(&missing, &cloudflare_context(), cloudflare_auth()).await;
    assert!(result.error_message.unwrap().contains("Cloudflare Workers AI error: No such model"));
}

// ---------------------------------------------------------------------------
// llama.cpp
// ---------------------------------------------------------------------------

type Next = Arc<dyn Fn(&str, u64) -> Vec<(String, f64)> + Send + Sync>;
type Tokenize = Arc<dyn Fn(&str) -> Vec<i64> + Send + Sync>;
type Template = Arc<dyn Fn(&Value) -> String + Send + Sync>;

fn char_tokens(content: &str) -> Vec<i64> {
    content.chars().map(|c| c as i64).collect()
}

fn word_tokens(content: &str) -> Vec<i64> {
    let mut tokens = Vec::new();
    let mut rest = content;
    while !rest.is_empty() {
        let (part, next) = match rest.find('\n') {
            Some(0) => ("\n", &rest[1..]),
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        match part {
            "Yes" => tokens.push(89),
            "No" => tokens.push(78),
            other => tokens.extend(char_tokens(other)),
        }
        rest = next;
    }
    tokens
}

async fn fake_llama(next: Option<Next>, tokenize: Option<Tokenize>, template: Option<Template>) -> MockServer {
    MockServer::start(move |request: &Request, _| {
        let body = request.json();
        match request.path_only() {
            "/tokenize" => {
                let content = body["content"].as_str().unwrap_or_default();
                let tokens = match &tokenize {
                    Some(tokenize) => tokenize(content),
                    None => char_tokens(content),
                };
                Response::json(200, json!({ "tokens": tokens }))
            }
            "/apply-template" => {
                let prompt = match &template {
                    Some(template) => template(&body["messages"]),
                    None => {
                        let messages = body["messages"].as_array().cloned().unwrap_or_default();
                        let rendered: String = messages
                            .iter()
                            .map(|m| format!("<|{}|>\n{}\n", m["role"].as_str().unwrap_or_default(), m["content"].as_str().unwrap_or_default()))
                            .collect();
                        format!("{rendered}<|assistant|>\n")
                    }
                };
                Response::json(200, json!({ "prompt": prompt }))
            }
            "/completion" => {
                let entries = match &next {
                    Some(next) => next(body["prompt"].as_str().unwrap_or_default(), body["n_probs"].as_u64().unwrap_or(0)),
                    None => vec![("A".into(), -0.1), ("B".into(), -2.5)],
                };
                let top: Vec<Value> = entries
                    .iter()
                    .map(|(token, logprob)| json!({ "id": token.chars().next().map(|c| c as i64), "token": token, "bytes": [], "logprob": logprob }))
                    .collect();
                Response::json(200, json!({ "content": "A", "completion_probabilities": [{ "id": 65, "token": "A", "top_logprobs": top }] }))
            }
            _ => Response::text(404, "not found"),
        }
    })
    .await
}

fn llama_model(server: &MockServer) -> ClassifierModel {
    serde_json::from_value(json!({
        "type": "classifier", "id": "qwen", "name": "qwen", "api": "llama-cpp-classify", "provider": "llama.cpp",
        "baseUrl": format!("{}/v1", server.url), "input": ["text"], "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }, "contextWindow": 32768
    }))
    .unwrap()
}

fn llama_context() -> ClassifierContext {
    serde_json::from_value(json!({
        "state": { "message": "Help! My payouts have been failing for 3 days." },
        "questions": {
            "team": { "type": "choice", "instructions": "Which team should handle this?", "criteria": { "billing": "Payments and refunds", "technical": "Bugs and outages", "sales": "" } },
            "urgent": { "type": "bool", "instructions": "Does this convey urgency?", "criteria": { "true": "The user needs help soon", "false": "No time pressure" } },
            "severity": { "type": "score", "instructions": "How severe is this?", "criteria": ["low", "medium", "high"] }
        }
    }))
    .unwrap()
}

fn pick_context() -> ClassifierContext {
    serde_json::from_value(json!({ "state": {}, "questions": { "pick": { "type": "choice", "instructions": "Pick one", "criteria": { "a": "", "b": "" } } } })).unwrap()
}

fn answer_by_prompt(prompt: &str, _depth: u64) -> Vec<(String, f64)> {
    let pairs: &[(&str, f64)] = if prompt.contains("Answer Yes or No.") {
        &[("Y", -0.05), ("N", -3.0)]
    } else if prompt.contains("Answer with one level number.") {
        &[("2", -0.2), ("1", -1.8), ("0", -4.0)]
    } else {
        &[("B", -0.3), ("A", -1.5), ("C", -3.0)]
    };
    pairs.iter().map(|(t, l)| (t.to_string(), *l)).collect()
}

fn completion_depths(server: &MockServer) -> Vec<u64> {
    server.requests().iter().filter(|r| r.path == "/completion").map(|r| r.json()["n_probs"].as_u64().unwrap()).collect()
}

#[tokio::test]
async fn llama_answers_from_label_logprobs() {
    let server = fake_llama(Some(Arc::new(answer_by_prompt)), Some(Arc::new(word_tokens)), None).await;
    let model = llama_model(&server);
    let result = llama_cpp_classify::classify(&model, &llama_context(), key("local")).await;
    assert_eq!(result.error_message, None);
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    let choice = label_probabilities(&[-1.5, -0.3, -3.0], 1.0);
    let mut probabilities = IndexMap::new();
    probabilities.insert("billing".to_string(), choice[0]);
    probabilities.insert("technical".to_string(), choice[1]);
    probabilities.insert("sales".to_string(), choice[2]);
    assert_eq!(result.answers["team"], ClassifierAnswer::Choice { choice: "technical".into(), probabilities, confidence: peak_confidence(&choice) });
    assert_eq!(result.answers["urgent"], ClassifierAnswer::Bool { probability: label_probabilities(&[-0.05, -3.0], 1.0)[0] });
    let levels = label_probabilities(&[-4.0, -1.8, -0.2], 1.0);
    assert_eq!(result.answers["severity"], ClassifierAnswer::Score { score: 0.0 * levels[0] + levels[1] + 2.0 * levels[2], confidence: peak_confidence(&levels) });
    for request in server.requests() {
        assert_eq!(request.json()["model"], "qwen");
        assert_eq!(request.header("authorization"), Some("Bearer local"));
    }
    let completion = server.requests().into_iter().find(|r| r.path == "/completion").unwrap().json();
    assert_eq!((completion["n_predict"].clone(), completion["n_probs"].clone()), (json!(1), json!(256)));
    assert_eq!(completion["post_sampling_probs"], false);
    assert_eq!(completion["cache_prompt"], true);
    let template = server.requests().into_iter().find(|r| r.path == "/apply-template").unwrap().json();
    assert_eq!(template["chat_template_kwargs"], json!({ "enable_thinking": false }));
}

#[test]
fn llama_renders_questions_with_repeated_state() {
    let context = llama_context();
    let rendered = render_question(&context, "team").unwrap();
    let state = "State:\n{\n \"message\": \"Help! My payouts have been failing for 3 days.\"\n}";
    assert_eq!(rendered.labels, ["A", "B", "C"]);
    assert_eq!(rendered.keys, ["billing", "technical", "sales"]);
    let expected = [
        state,
        "",
        "Task: answer each of the following questions about the state.",
        "",
        "Question: Which team should handle this?",
        "",
        "Options:",
        "- billing: Payments and refunds",
        "- technical: Bugs and outages",
        "- sales",
        "",
        "Question: Does this convey urgency?",
        "",
        "Yes means: The user needs help soon",
        "No means: No time pressure",
        "",
        "Question: How severe is this?",
        "",
        "Levels:",
        "0. low",
        "1. medium",
        "2. high",
        "",
        state,
        "",
        "Question: Which team should handle this?",
        "",
        "Options:",
        "A. billing: Payments and refunds",
        "B. technical: Bugs and outages",
        "C. sales",
        "",
        "Answer with one letter.",
    ]
    .join("\n");
    assert_eq!(rendered.content, expected);
    let prefix = |id: &str| {
        let content = render_question(&context, id).unwrap().content;
        content[..content.rfind("Question:").unwrap()].to_string()
    };
    assert_eq!(prefix("urgent"), prefix("team"));
    assert_eq!(prefix("severity"), prefix("team"));
    assert!(render_question(&context, "urgent").unwrap().content.ends_with("No means: No time pressure\n\nAnswer Yes or No."));
    assert!(render_question(&context, "severity").unwrap().content.ends_with("2. high\n\nAnswer with one level number."));
}

#[tokio::test]
async fn llama_temperature_and_validation() {
    let server = fake_llama(Some(Arc::new(|_, _| vec![("A".into(), -0.1), ("B".into(), -2.5)])), None, None).await;
    let options = StreamOptions { temperature: Some(2.0), ..Default::default() };
    let result = llama_cpp_classify::classify(&llama_model(&server), &pick_context(), options).await;
    let expected = label_probabilities(&[-0.1 / 2.0, -2.5 / 2.0], 1.0);
    let ClassifierAnswer::Choice { probabilities, .. } = &result.answers["pick"] else { panic!("{:?}", result.error_message) };
    assert_eq!((probabilities["a"], probabilities["b"]), (expected[0], expected[1]));
    assert_eq!(label_probabilities(&[-0.1, -2.5], 2.0), expected);

    let server = fake_llama(None, None, None).await;
    let options = StreamOptions { temperature: Some(0.0), ..Default::default() };
    let result = llama_cpp_classify::classify(&llama_model(&server), &llama_context(), options).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert!(result.error_message.unwrap().contains("Temperature must be a positive number, got 0"));
    assert!(server.requests().is_empty());

    let criteria: serde_json::Map<String, Value> = (0..63).map(|i| (format!("option{i}"), json!(""))).collect();
    let too_many: ClassifierContext = serde_json::from_value(json!({ "state": {}, "questions": { "pick": { "type": "choice", "instructions": "Pick", "criteria": criteria } } })).unwrap();
    let too_few: ClassifierContext = serde_json::from_value(json!({ "state": {}, "questions": { "rate": { "type": "score", "instructions": "Rate", "criteria": ["only"] } } })).unwrap();
    let result = llama_cpp_classify::classify(&llama_model(&server), &too_many, StreamOptions::default()).await;
    assert!(result.error_message.unwrap().contains("A choice question needs 2 to 62 options, got 63"));
    let result = llama_cpp_classify::classify(&llama_model(&server), &too_few, StreamOptions::default()).await;
    assert!(result.error_message.unwrap().contains("A score question needs 2 to 10 levels, got 1"));
    assert!(server.requests().is_empty());

    let mut other = llama_model(&server);
    other.api = "typesafe-system-one".into();
    let result = llama_cpp_classify::classify(&other, &llama_context(), StreamOptions::default()).await;
    assert!(result.error_message.unwrap().contains("Unsupported classifier API: typesafe-system-one"));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn llama_escalates_readout_depth() {
    let deep = fake_llama(Some(Arc::new(|_, depth| if depth < 4096 { vec![("A".into(), -0.1)] } else { vec![("A".into(), -0.1), ("B".into(), -9.0)] })), None, None).await;
    let recovered = llama_cpp_classify::classify(&llama_model(&deep), &pick_context(), StreamOptions::default()).await;
    assert_eq!(recovered.stop_reason, ClassifierStopReason::Stop, "{:?}", recovered.error_message);
    assert_eq!(completion_depths(&deep), [256, 4096]);

    let never = fake_llama(Some(Arc::new(|_, _| vec![("A".into(), -0.1)])), None, None).await;
    let failed = llama_cpp_classify::classify(&llama_model(&never), &pick_context(), StreamOptions::default()).await;
    assert_eq!(failed.stop_reason, ClassifierStopReason::Error);
    assert!(failed.answers.is_empty());
    assert!(failed.error_message.unwrap().contains("did not rank labels B for pick within the top 32768 tokens"));
    assert_eq!(completion_depths(&never), [256, 4096, 32768]);
}

#[tokio::test]
async fn llama_templates_tokens_and_caching() {
    let server = fake_llama(None, None, Some(Arc::new(|_| "<|assistant|>\n<think>".to_string()))).await;
    llama_cpp_classify::classify(&llama_model(&server), &pick_context(), StreamOptions::default()).await;
    let completion = server.requests().into_iter().find(|r| r.path == "/completion").unwrap().json();
    assert_eq!(completion["prompt"], "<|assistant|>\n<think></think>");

    let merging = fake_llama(None, Some(Arc::new(|content: &str| if content.starts_with('\n') && content.chars().count() > 1 { vec![1000] } else { char_tokens(content) })), None).await;
    let merged = llama_cpp_classify::classify(&llama_model(&merging), &pick_context(), StreamOptions::default()).await;
    assert_eq!(merged.stop_reason, ClassifierStopReason::Stop, "{:?}", merged.error_message);

    let split = fake_llama(None, None, None).await;
    let bool_context: ClassifierContext = serde_json::from_value(json!({ "state": {}, "questions": { "ok": { "type": "bool", "instructions": "OK?", "criteria": { "true": "", "false": "" } } } })).unwrap();
    let result = llama_cpp_classify::classify(&llama_model(&split), &bool_context, StreamOptions::default()).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert!(result.error_message.unwrap().contains("Label \"Yes\" is not a single token for qwen"));

    let cached = fake_llama(None, None, None).await;
    let model = llama_model(&cached);
    llama_cpp_classify::classify(&model, &pick_context(), StreamOptions::default()).await;
    let first = cached.requests().iter().filter(|r| r.path == "/tokenize").count();
    llama_cpp_classify::classify(&model, &pick_context(), StreamOptions::default()).await;
    assert!(first > 0);
    assert_eq!(cached.requests().iter().filter(|r| r.path == "/tokenize").count(), first);
}

#[tokio::test]
async fn llama_hooks_errors_and_cancellation() {
    let server = fake_llama(None, None, None).await;
    let payloads = Arc::new(Mutex::new(Vec::<Value>::new()));
    let statuses = Arc::new(Mutex::new(Vec::<u16>::new()));
    let (payload_sink, status_sink) = (payloads.clone(), statuses.clone());
    let options = StreamOptions {
        on_payload: Some(Arc::new(move |payload: Value, _: Model| {
            payload_sink.lock().push(payload.clone());
            let mut next = payload.as_object().cloned().unwrap();
            next.insert("id_slot".into(), json!(1));
            Box::pin(async move { Some(Value::Object(next)) })
        })),
        on_response: Some(Arc::new(move |response: ProviderResponse, _: Model| {
            status_sink.lock().push(response.status);
            Box::pin(async {})
        })),
        ..Default::default()
    };
    llama_cpp_classify::classify(&llama_model(&server), &pick_context(), options).await;
    assert_eq!(payloads.lock().len(), 1);
    assert_eq!(payloads.lock()[0]["n_predict"], 1);
    assert_eq!(*statuses.lock(), [200]);
    assert_eq!(server.requests().into_iter().find(|r| r.path == "/completion").unwrap().json()["id_slot"], 1);

    let failing = MockServer::sequence(vec![Response::text(400, r#"{"error":{"message":"context overflow"}}"#)]).await;
    let result = llama_cpp_classify::classify(&llama_model(&failing), &llama_context(), StreamOptions { max_retries: Some(0), ..Default::default() }).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    let message = result.error_message.unwrap();
    assert!(message.contains("llama.cpp error (400)"), "{message}");
    assert!(message.contains("context overflow"));

    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();
    let result = llama_cpp_classify::classify(&llama_model(&server), &llama_context(), StreamOptions { signal: Some(signal), ..Default::default() }).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Aborted);
}

#[test]
fn llama_helpers() {
    assert_eq!(llama_server_root("http://127.0.0.1:8080/v1/"), "http://127.0.0.1:8080");
    assert_eq!(llama_server_root("https://example.com/prefix/v1"), "https://example.com/prefix");
    assert_eq!(llama_server_root("http://127.0.0.1:8080"), "http://127.0.0.1:8080");
    assert!((peak_confidence(&[0.89, 0.06, 0.05]) - 0.835).abs() < 1e-9);
    assert_eq!(peak_confidence(&[0.5, 0.5]), 0.0);
    assert_eq!(peak_confidence(&[1.0, 0.0, 0.0]), 1.0);
    let question: ClassifierQuestion = serde_json::from_value(json!({ "type": "score", "instructions": "", "criteria": ["a", "b", "c"] })).unwrap();
    let keys = ["0".to_string(), "1".to_string(), "2".to_string()];
    let answer = answer_from_probabilities(&question, &keys, &[0.2, 0.3, 0.5]);
    let ClassifierAnswer::Score { score, confidence } = answer else { panic!() };
    assert!((score - 1.3).abs() < 1e-12);
    assert_eq!(confidence, peak_confidence(&[0.2, 0.3, 0.5]));
}
