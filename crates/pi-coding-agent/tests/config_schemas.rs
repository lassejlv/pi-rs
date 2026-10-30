//! Port of `test/config-schemas.test.ts`. The Rust port embeds the TS package's generated schema
//! artifacts (`core/schemas/*.schema.json`) instead of rendering them; the artifact test checks
//! that they are the committed TS artifacts (when the TS checkout is available). The theme schema
//! is built in Rust from the theme tokens. Documents are validated with pi-ai's TypeBox-compatible
//! checker.

use std::path::{Path, PathBuf};

use pi_ai::utils::validation::SchemaValidator;
use pi_coding_agent::core::keybindings::KeybindingsManager;
use pi_coding_agent::core::keybindings_schema::KEYBINDINGS_SCHEMA_ARTIFACT;
use pi_coding_agent::core::model_config::{MODELS_SCHEMA_ARTIFACT, ModelConfig};
use pi_coding_agent::core::settings_manager::SettingsManager;
use pi_coding_agent::core::settings_schema::{SETTINGS_SCHEMA, SETTINGS_SCHEMA_ARTIFACT};
use pi_coding_agent::modes::interactive::theme::theme_schema::{THEME_JSON_SCHEMA, validate_theme_json};
use serde_json::{Value, json};

const DEFAULT_SOURCE: &str = "/tmp/claude-0/-home-user-pi-rs/3420df64-2251-57a4-b9aa-4eb6ba35ed96/scratchpad/src/pi";

fn ts_package_dir() -> Option<PathBuf> {
    let root = std::env::var("PI_TS_SOURCE").unwrap_or_else(|_| DEFAULT_SOURCE.to_string());
    let dir = Path::new(&root).join("packages/coding-agent");
    dir.is_dir().then_some(dir)
}

fn schema_url(name: &str) -> String {
    format!("https://pi.dev/schemas/{name}.schema.json")
}

fn artifact(content: &str) -> Value {
    serde_json::from_str(content).unwrap()
}

fn check(schema: &Value, value: Value) -> bool {
    SchemaValidator::new(schema).check(&value)
}

#[test]
fn matches_the_committed_artifacts() {
    let Some(dir) = ts_package_dir() else {
        eprintln!("skipping: reference TypeScript sources not found (set PI_TS_SOURCE)");
        return;
    };
    for (name, embedded) in [
        ("models", MODELS_SCHEMA_ARTIFACT),
        ("settings", SETTINGS_SCHEMA_ARTIFACT),
        ("keybindings", KEYBINDINGS_SCHEMA_ARTIFACT),
    ] {
        let committed = std::fs::read_to_string(dir.join(format!("schemas/{name}.schema.json"))).unwrap();
        assert_eq!(embedded.get(), committed, "schemas/{name}.schema.json is stale");
    }
    // The theme schema is generated from the theme tokens; compare it with the inlined artifact.
    let mut committed: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("schemas/theme.schema.json")).unwrap()).unwrap();
    let defs = committed.as_object_mut().unwrap().remove("$defs").unwrap_or(Value::Null);
    for key in ["$schema", "$id", "$comment"] {
        committed.as_object_mut().unwrap().remove(key);
    }
    fn inline(value: &mut Value, defs: &Value) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(reference)) = map.get("$ref").cloned()
                    && let Some(name) = reference.strip_prefix("#/$defs/")
                {
                    map.remove("$ref");
                    let mut def = defs[name].clone();
                    inline(&mut def, defs);
                    if let Value::Object(def) = def {
                        for (k, v) in def {
                            map.entry(k).or_insert(v);
                        }
                    }
                }
                for v in map.values_mut() {
                    inline(v, defs);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| inline(v, defs)),
            _ => {}
        }
    }
    inline(&mut committed, &defs);
    let ours = THEME_JSON_SCHEMA.clone();
    assert_eq!(ours["properties"]["colors"]["required"], committed["properties"]["colors"]["required"]);
    let ours_keys: Vec<&String> = ours["properties"]["colors"]["properties"].as_object().unwrap().keys().collect();
    let committed_keys: Vec<&String> = committed["properties"]["colors"]["properties"].as_object().unwrap().keys().collect();
    assert_eq!(ours_keys, committed_keys);
}

#[test]
fn preserves_editor_guidance_in_the_theme_schema() {
    let schema = &*THEME_JSON_SCHEMA;
    assert_eq!(schema["title"], "Pi Coding Agent Theme");
    assert_eq!(schema["description"], "Theme schema for Pi coding agent");
    let colors = &schema["properties"]["colors"];
    assert!(colors["description"].as_str().unwrap_or_default().contains("use compatible fallbacks"), "{}", colors["description"]);
    assert!(colors["properties"]["scrollbarTrack"]["description"].as_str().unwrap().contains("falls back to muted"));
    assert_eq!(colors["properties"]["accent"]["description"], "Primary accent color (logo, selected items, cursor)");
    assert!(
        schema["properties"]["export"]["description"].as_str().unwrap_or_default().contains("defaults derived from userMessageBg"),
        "{}",
        schema["properties"]["export"]["description"]
    );
    let color_value = &colors["properties"]["accent"];
    assert!(
        color_value["anyOf"][0]["description"].as_str().unwrap_or_default().contains("Hex color"),
        "{color_value}"
    );
}

#[test]
fn preserves_moved_model_and_settings_guidance() {
    let models = artifact(MODELS_SCHEMA_ARTIFACT.get());
    let cost = &models["$defs"]["ModelCost"]["properties"];
    assert!(cost["input"]["description"].as_str().unwrap().contains("USD per million tokens"));
    assert!(cost["tiers"]["description"].as_str().unwrap().contains("highest matching input threshold"));
    assert!(
        models["$defs"]["ModelInputLimits"]["properties"]["maxRequestBytes"]["description"]
            .as_str()
            .unwrap()
            .contains("serialized provider request size")
    );
    let settings = artifact(SETTINGS_SCHEMA_ARTIFACT.get());
    let properties = &settings["properties"];
    assert!(properties["quietStartup"]["description"].as_str().unwrap().contains("hide all startup output"));
    assert!(properties["defaultTools"]["description"].as_str().unwrap().contains("+name and -name"));
    assert!(properties["fullscreenWheelScrollLines"]["description"].as_str().unwrap().contains("1 to 100"));
}

#[test]
fn validates_representative_documents() {
    let models = artifact(MODELS_SCHEMA_ARTIFACT.get());
    assert!(check(
        &models,
        json!({ "providers": { "local": {
            "compat": {
                "zaiToolStream": true,
                "thinkingTokenBudgetField": "thinking_budget",
                "chatTemplateKwargs": { "budget": { "$var": "thinking.budget" } },
                "supportsMidConvoSystemMessages": true
            },
            "models": [{
                "id": "model",
                "inputLimits": { "maxRequestBytes": 1024, "images": { "maxPerRequest": 2 } },
                "promptCache": { "short": 300 }
            }]
        } } })
    ));
    assert!(!check(&models, json!({ "providers": { "local": { "models": [{ "id": "" }] } } })));

    let settings = artifact(SETTINGS_SCHEMA_ARTIFACT.get());
    assert!(check(
        &settings,
        json!({
            "theme": "dark", "cacheWarming": "idle", "extensionSetting": { "enabled": true }, "queueMode": "all",
            "websockets": true, "skills": { "enableSkillCommands": true, "customDirectories": ["./skills"] },
            "retry": { "maxDelayMs": 1000 }
        })
    ));
    assert!(!check(&settings, json!({ "cacheWarming": "always" })));

    let keybindings = artifact(KEYBINDINGS_SCHEMA_ARTIFACT.get());
    assert!(check(&keybindings, json!({ "app.session.new": "ctrl+n", "extension.action": ["alt+x"] })));
    assert!(!check(&keybindings, json!({ "app.session.new": 42 })));
}

#[test]
fn rejects_non_positive_model_token_limits() {
    let models = artifact(MODELS_SCHEMA_ARTIFACT.get());
    for field in ["contextWindow", "maxTokens"] {
        for value in [0, -1] {
            assert!(
                !check(&models, json!({ "providers": { "local": { "models": [{ "id": "model", field: value }] } } })),
                "models[].{field}={value}"
            );
            assert!(
                !check(&models, json!({ "providers": { "local": { "modelOverrides": { "model": { field: value } } } } })),
                "modelOverrides.{field}={value}"
            );
        }
    }
    assert!(check(
        &models,
        json!({ "providers": { "local": {
            "models": [{ "id": "model", "contextWindow": 1, "maxTokens": 1 }],
            "modelOverrides": { "model": { "contextWindow": 1, "maxTokens": 1 } }
        } } })
    ));
}

// "validates keybinding syntax" is `core::key_id_schema::tests::validates_keybinding_syntax`.

#[test]
fn rejects_settings_values_that_runtime_accessors_reject() {
    let schema = &*SETTINGS_SCHEMA;
    for invalid in [
        json!({ "compaction": { "reserveTokens": -1 } }),
        json!({ "compaction": { "keepRecentTokens": 1.5 } }),
        json!({ "compaction": { "modelOverrides": { "provider/model": { "reserveTokens": 9_007_199_254_740_992u64 } } } }),
        json!({ "httpIdleTimeoutMs": -1 }),
        json!({ "websocketConnectTimeoutMs": -1 }),
        json!({ "httpIdleTimeoutMs": "bogus" }),
        json!({ "websocketConnectTimeoutMs": "30000" }),
        json!({ "codemode": { "inlineBudget": -1 } }),
    ] {
        assert!(!check(schema, invalid.clone()), "{invalid}");
    }
    assert!(check(
        schema,
        json!({
            "compaction": {
                "reserveTokens": 0,
                "keepRecentTokens": 9_007_199_254_740_991u64,
                "modelOverrides": { "provider/model": { "reserveTokens": 0 } }
            },
            "httpIdleTimeoutMs": 0,
            "websocketConnectTimeoutMs": "disabled",
            "codemode": { "inlineBudget": 0 }
        })
    ));
}

#[test]
fn describes_legacy_settings_migrations_accurately() {
    let schema = artifact(SETTINGS_SCHEMA_ARTIFACT.get());
    assert!(
        schema["properties"]["retry"]["properties"]["maxDelayMs"]["description"]
            .as_str()
            .unwrap()
            .contains("provider.maxRetryDelayMs")
    );
    assert_eq!(schema["properties"]["queueMode"]["description"], "Legacy setting migrated to steeringMode.");
}

// "accepts custom compatibility settings and rejects malformed known fields" is
// `core::model_config::tests::accepts_custom_compat_and_rejects_malformed_known_fields`.

#[tokio::test]
async fn validates_schema_in_models_json_at_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models.json");
    let path_str = path.to_string_lossy().into_owned();
    std::fs::write(
        &path,
        json!({ "$schema": schema_url("models"), "providers": { "demo": { "baseUrl": "http://localhost:8080/v1", "models": [{ "id": "demo" }] } } })
            .to_string(),
    )
    .unwrap();
    let config = ModelConfig::load(Some(&path_str)).await;
    assert_eq!(config.get_error(), None);
    let provider = config.get_provider("demo").unwrap();
    assert_eq!(provider.models.as_ref().unwrap()[0].id, "demo");

    std::fs::write(&path, json!({ "$schema": schema_url("models"), "providers": { "demo": { "models": [{ "id": "" }] } } }).to_string())
        .unwrap();
    assert!(ModelConfig::load(Some(&path_str)).await.get_error().unwrap_or_default().contains("Invalid models.json schema"));

    std::fs::write(&path, json!({ "$schema": 42, "providers": {} }).to_string()).unwrap();
    assert!(ModelConfig::load(Some(&path_str)).await.get_error().unwrap_or_default().contains("$schema"));
}

#[test]
fn accepts_schema_in_settings_json_and_preserves_unknown_settings() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.json"),
        json!({ "$schema": schema_url("settings"), "theme": "light", "extensionSetting": { "enabled": true } }).to_string(),
    )
    .unwrap();
    let d = dir.path().to_string_lossy().into_owned();
    let manager = SettingsManager::create(&d, Some(&d), None);
    assert_eq!(manager.get_theme().as_deref(), Some("light"));
    let global = Value::Object(manager.get_global_settings_json());
    assert_eq!(global["$schema"], json!(schema_url("settings")));
    assert_eq!(global["extensionSetting"], json!({ "enabled": true }));
}

#[test]
fn accepts_schema_in_keybindings_json_without_treating_it_as_an_action() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("keybindings.json"),
        json!({ "$schema": schema_url("keybindings"), "app.session.new": "ctrl+n", "extension.invalid": "ctrl+not-a-key" })
            .to_string(),
    )
    .unwrap();
    let manager = KeybindingsManager::create(Some(&dir.path().to_string_lossy()));
    let find = |config: Vec<(String, Option<pi_tui::keybindings::KeySpec>)>, name: &str| {
        config.into_iter().find(|(k, _)| k == name).and_then(|(_, v)| v).map(|k| k.to_vec())
    };
    let user = manager.get_user_bindings();
    assert_eq!(user.len(), 1);
    assert_eq!(find(user, "app.session.new"), Some(vec!["ctrl+n".to_string()]));
    assert_eq!(find(manager.get_effective_config(), "app.session.new"), Some(vec!["ctrl+n".to_string()]));
}

#[test]
fn accepts_schema_in_themes_and_rejects_malformed_themes() {
    let built_in: Value = serde_json::from_str(include_str!("../src/modes/interactive/theme/dark.json")).unwrap();
    let with = |patch: Value| {
        let mut theme = built_in.clone();
        for (k, v) in patch.as_object().unwrap() {
            theme[k] = v.clone();
        }
        theme
    };
    assert_eq!(
        validate_theme_json("custom", with(json!({ "$schema": schema_url("theme"), "name": "custom" }))).unwrap().name,
        "custom"
    );
    assert!(
        validate_theme_json("invalid", with(json!({ "name": "invalid/name" }))).unwrap_err().to_string().contains("theme names cannot contain")
    );
    assert!(
        validate_theme_json("invalid", with(json!({ "colors": {} }))).unwrap_err().to_string().contains("Missing required color tokens")
    );
    let mut misspelled = built_in.clone();
    misspelled["colors"]["scrollbarThmb"] = json!("");
    for invalid in [with(json!({ "unexpected": true })), misspelled, with(json!({ "export": { "unexpected": "" } }))] {
        let error = validate_theme_json("invalid", invalid).unwrap_err().to_string();
        assert!(error.to_lowercase().contains("additional properties"), "{error}");
    }
}
