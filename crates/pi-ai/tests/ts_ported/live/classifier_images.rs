//! Live port of classifier-images-live.test.ts: image classification through System One
//! services. Each case sends a solid red and a solid blue PNG in separate requests, so a service
//! that drops the image cannot pass by chance.

use pi_ai::providers::builtin::typesafe_provider;
use pi_ai::providers::cloudflare::cloudflare_workers_ai_provider;
use pi_ai::providers::openrouter::openrouter_provider;
use pi_ai::*;
use serde_json::json;

use super::*;

const FILE: &str = "classifier-images-live.test.ts";

// 512x512 solid PNGs from the TypeSafe SDK's live image tests.
const RED: &str = "iVBORw0KGgoAAAANSUhEUgAAAgAAAAIAAQMAAADOtka5AAAABlBMVEX/AAAAAABBoxIDAAAANklEQVR42u3BAQEAAACCIP+vbkhAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB8G4IAAAFjdVCkAAAAAElFTkSuQmCC";
const BLUE: &str = "iVBORw0KGgoAAAANSUhEUgAAAgAAAAIAAQMAAADOtka5AAAABlBMVEUAAP8AAAB7Yr8+AAAANklEQVR42u3BAQEAAACCIP+vbkhAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB8G4IAAAFjdVCkAAAAAElFTkSuQmCC";

fn color_context(data: &str) -> ClassifierContext {
    serde_json::from_value(json!({
        "state": { "task": "Name the color of the attached image" },
        "images": [{ "type": "image", "data": data, "mimeType": "image/png" }],
        "questions": {
            "color": {
                "type": "choice",
                "instructions": "What color is the attached image?",
                "criteria": { "red": "Red", "blue": "Blue", "green": "Green" }
            }
        }
    }))
    .unwrap()
}

fn models() -> Models {
    let models = create_models(CreateModelsOptions::default());
    models.set_provider(typesafe_provider());
    models.set_provider(cloudflare_workers_ai_provider());
    models.set_provider(openrouter_provider());
    models
}

async fn expect_colors(provider: &str, id: &str, options: StreamOptions) {
    let models = models();
    let Some(AnyModel::Classifier(model)) = models.get_model_of_type(ModelType::Classifier, provider, id) else {
        panic!("missing {provider}/{id}");
    };
    assert!(model.input.contains(&ModelInputModality::Image));
    for (data, color) in [(RED, "red"), (BLUE, "blue")] {
        let result = models.classify(&model, &color_context(data), options.clone()).await;
        assert_eq!(result.error_message, None, "{provider}/{id}");
        assert!(
            matches!(&result.answers["color"], ClassifierAnswer::Choice { choice, .. } if choice == color),
            "{provider}/{id}: {:?}",
            result.answers
        );
    }
}

/// One `it.each` block: every id runs when the gate opens, as in TS.
async fn run_ids(name: &str, gate: Gate, provider: &str, ids: &[&str], options: fn() -> StreamOptions) {
    if open_gate(&gate).await.is_none() {
        println!("[{FILE}] skip {name} ({gate:?} not configured)");
        return;
    }
    for id in ids {
        expect_colors(provider, id, options()).await;
        println!("[{FILE}] ok   {name}: {id}");
    }
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn typesafe_image_classification() {
    run_ids("TypeSafe image classification", Gate::Env(&["TYPESAFE_API_KEY"]), "typesafe", &["jev-latest"], || {
        StreamOptions { api_key: env("TYPESAFE_API_KEY"), ..Default::default() }
    })
    .await;
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn cloudflare_workers_ai_image_classification() {
    run_ids(
        "Cloudflare Workers AI image classification",
        Gate::CloudflareWorkers,
        "cloudflare-workers-ai",
        &["@cf/cloudflare/clef", "@cf/cloudflare/clef-flash", "@cf/cloudflare/clef-omni", "typesafe/jev", "typesafe/jev-1.14.0"],
        || {
            let mut provider_env = ProviderEnv::new();
            provider_env.insert("CLOUDFLARE_ACCOUNT_ID".into(), env("CLOUDFLARE_ACCOUNT_ID").unwrap_or_default());
            StreamOptions { api_key: env("CLOUDFLARE_API_KEY"), env: Some(provider_env), ..Default::default() }
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn openrouter_image_classification() {
    run_ids(
        "OpenRouter image classification",
        Gate::Env(&["OPENROUTER_API_KEY"]),
        "openrouter",
        &["typesafe/jev-1.14", "cloudflare/clef-flash"],
        || StreamOptions { api_key: env("OPENROUTER_API_KEY"), ..Default::default() },
    )
    .await;
}
