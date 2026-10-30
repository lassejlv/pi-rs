//! The typed forms of `AgentChange`, stream options, retry policy, compaction settings and
//! `afterFailedRun` convert losslessly to the JSON the entry points take, and behave like it.
use pi_durable::{
    harness::{
        agent::{AgentState, apply_change},
        compaction::compaction_policy,
        settings::{HarnessSettings, resolve_settings},
        typed::Change,
    },
    *,
};

fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
fn object(text: &str) -> JsonObject {
    JsonObject::try_from_value(json(text)).unwrap()
}

#[test]
fn agent_changes_round_trip_and_apply_like_the_json_they_replace() {
    let cases = [
        r#"{"model":{"provider":"faux","modelId":"faux-1"},"thinkingLevel":"high","instructions":"Be brief","cwd":"/work"}"#,
        r#"{"model":null,"thinkingLevel":null,"instructions":null,"cwd":null}"#,
        r#"{"extensions":[{"name":"a"},{"name":"b"}],"tools":[{"name":"read"}],"modelTools":[{"name":"bash"}]}"#,
        r#"{"extensions":{"add":[{"name":"a"}],"remove":[{"name":"b"}]},"tools":{"remove":[{"name":"write"}]},"modelTools":{"remove":[{"name":"x"}],"add":[{"name":"y"}]}}"#,
        r#"{"extensions":null,"tools":null,"modelTools":null}"#,
        r#"{}"#,
    ];
    let stored = r#"{"model":{"provider":"old","modelId":"old-1"},"thinkingLevel":"low","instructions":"old","cwd":"/old","extensions":["z"],"tools":["t"],"modelTools":["m"],"custom":1}"#;
    for text in cases {
        let wire = object(text);
        let typed = AgentChange::try_from(&wire).unwrap();
        assert_eq!(JsonObject::try_from(&typed).unwrap(), wire, "{text}");
        let (mut from_json, mut from_typed) =
            (AgentState(object(stored)), AgentState(object(stored)));
        apply_change(&mut from_json, &wire).unwrap();
        apply_change(&mut from_typed, &JsonObject::try_from(&typed).unwrap()).unwrap();
        assert_eq!(from_json, from_typed, "{text}");
    }
}

#[test]
fn agent_changes_distinguish_unset_clear_and_set_and_ignore_unknown_keys() {
    let typed = AgentChange::try_from(&object(
        r#"{"instructions":null,"cwd":"/w","thinkingLevel":"xhigh","somethingElse":true}"#,
    ))
    .unwrap();
    assert_eq!(typed.instructions, Change::Clear);
    assert_eq!(typed.cwd, Change::Set("/w".into()));
    assert_eq!(typed.model, Change::Unset);
    assert_eq!(typed.tools, Change::Unset);
    // Unknown keys have no effect upstream and are not carried.
    assert_eq!(
        JsonObject::try_from(&typed).unwrap(),
        object(r#"{"thinkingLevel":"xhigh","instructions":null,"cwd":"/w"}"#)
    );
    // Extra fields of named choices are ignored, as only `name` is read.
    let named =
        AgentChange::try_from(&object(r#"{"tools":[{"name":"read","description":"d"}]}"#)).unwrap();
    assert_eq!(
        named.tools,
        Change::Set(ToolSelection::List(vec!["read".into()]))
    );
    // Shapes upstream's types forbid are rejected.
    for bad in [
        r#"{"thinkingLevel":"loud"}"#,
        r#"{"tools":["read"]}"#,
        r#"{"model":{"provider":"p"}}"#,
        r#"{"extensions":{"add":"a"}}"#,
    ] {
        assert!(AgentChange::try_from(&object(bad)).is_err(), "{bad}");
    }
    let built = AgentChange {
        instructions: Change::Set("hi".into()),
        extensions: Change::Set(NameSelection::Edit {
            add: Some(vec!["a".into()]),
            remove: None,
        }),
        ..Default::default()
    };
    assert_eq!(
        JsonObject::try_from(&built).unwrap(),
        object(r#"{"extensions":{"add":[{"name":"a"}]},"instructions":"hi"}"#)
    );
}

#[test]
fn stream_options_keep_every_field_including_unknown_ones() {
    for text in [
        r#"{}"#,
        r#"{"transport":"websocket-cached","timeoutMs":30000,"maxRetries":2,"maxRetryDelayMs":5000,"headers":{"x-a":"b"},"metadata":{"user":"u","n":1},"cacheRetention":"long","deferred":{"window":"1h"}}"#,
        r#"{"deferred":true,"transport":"sse","cacheRetention":"none"}"#,
        r#"{"temperature":0.5,"maxTokens":100,"sessionId":"s","vendor":{"k":[1,2]}}"#,
    ] {
        let wire = object(text);
        let typed = ConversationStreamOptions::try_from(&wire).unwrap();
        assert_eq!(JsonObject::try_from(&typed).unwrap(), wire, "{text}");
    }
    let typed =
        ConversationStreamOptions::try_from(&object(r#"{"timeoutMs":10,"vendor":1}"#)).unwrap();
    assert_eq!(typed.timeout_ms, Some(10.0));
    assert_eq!(typed.extra["vendor"], serde_json::json!(1));
    assert!(
        ConversationStreamOptions::try_from(&object(r#"{"transport":"carrier-pigeon"}"#)).is_err()
    );
    let settings = resolve_settings(HarnessSettings::default().with_stream(&typed).unwrap());
    assert_eq!(settings.stream_options().unwrap(), typed);
}

#[test]
fn retry_patches_merge_over_the_defaults_as_upstream_spreads_them() {
    let defaults = resolve_settings(HarnessSettings::default());
    let policy = defaults.retry_policy().unwrap();
    assert_eq!(
        (
            policy.enabled,
            policy.max_retries,
            policy.base_delay_ms,
            policy.max_agent_delay_ms
        ),
        (true, 3.0, 2000.0, Some(60000.0))
    );
    let patch = ConversationRetryPolicyPatch {
        max_retries: Some(5.0),
        extra: serde_json::json!({ "jitter": true })
            .as_object()
            .unwrap()
            .clone(),
        ..Default::default()
    };
    let settings = resolve_settings(HarnessSettings::default().with_retry(&patch).unwrap());
    assert_eq!(
        settings.retry.value(),
        &json(
            r#"{"enabled":true,"maxRetries":5,"baseDelayMs":2000,"maxAgentDelayMs":60000,"jitter":true}"#
        )
    );
    let typed = settings.retry_policy().unwrap();
    assert_eq!(
        (typed.max_retries, typed.extra["jitter"].clone()),
        (5.0, serde_json::json!(true))
    );
    // The full policy needs its required fields.
    assert!(ConversationRetryPolicy::try_from(&object(r#"{"enabled":true}"#)).is_err());
    assert_eq!(
        JsonObject::try_from(&patch).unwrap(),
        object(r#"{"maxRetries":5,"jitter":true}"#)
    );
}

#[test]
fn compaction_settings_apply_model_overrides_like_the_json_form() {
    let wire = object(
        r#"{"reserveTokens":1000,"modelOverrides":{"faux/faux-1":{"keepRecentTokens":50,"enabled":false}},"note":"kept"}"#,
    );
    let typed = CompactionSettings::try_from(&wire).unwrap();
    assert_eq!(typed.policy.reserve_tokens, Some(1000.0));
    assert_eq!(
        typed.model_overrides.as_ref().unwrap()["faux/faux-1"].keep_recent_tokens,
        Some(50.0)
    );
    assert_eq!(typed.extra["note"], serde_json::json!("kept"));
    assert_eq!(JsonObject::try_from(&typed).unwrap(), wire);
    let settings = resolve_settings(HarnessSettings::default().with_compaction(&typed).unwrap());
    let model = ModelRef {
        provider: "faux".into(),
        model_id: "faux-1".into(),
    };
    let policy = compaction_policy(&settings, &model).unwrap();
    assert_eq!(
        (
            policy.enabled,
            policy.reserve_tokens,
            policy.keep_recent_tokens,
            policy.background_tokens
        ),
        (false, 1000.0, 50.0, 32768.0)
    );
    let other = ModelRef {
        provider: "faux".into(),
        model_id: "faux-2".into(),
    };
    let policy = compaction_policy(&settings, &other).unwrap();
    assert_eq!((policy.enabled, policy.keep_recent_tokens), (true, 20000.0));
    assert_eq!(
        settings
            .compaction_settings()
            .unwrap()
            .policy
            .background_tokens,
        Some(32768.0)
    );
}

#[test]
fn after_failed_run_is_one_of_keep_continue_withdraw() {
    assert_eq!(AfterFailedRun::default(), AfterFailedRun::Keep);
    let defaults = resolve_settings(HarnessSettings::default());
    assert_eq!(
        defaults.after_failed_run_kind().unwrap(),
        AfterFailedRun::Keep
    );
    for (text, kind) in [
        ("keep", AfterFailedRun::Keep),
        ("continue", AfterFailedRun::Continue),
        ("withdraw", AfterFailedRun::Withdraw),
    ] {
        let settings = resolve_settings(HarnessSettings::default().with_after_failed_run(kind));
        assert!(settings.after_failed_run.equals_str(text));
        assert_eq!(settings.after_failed_run_kind().unwrap(), kind);
        assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(text));
    }
    let bad = resolve_settings(HarnessSettings {
        after_failed_run: Some("abort".into()),
        ..Default::default()
    });
    assert!(
        bad.after_failed_run_kind()
            .unwrap_err()
            .to_string()
            .contains("abort")
    );
}

#[test]
fn typed_changes_flow_into_the_existing_json_entry_points() {
    let change = AgentChange {
        model: Change::Set(ModelChange::from(ModelRef {
            provider: "faux".into(),
            model_id: "faux-1".into(),
        })),
        ..Default::default()
    };
    let options = ConversationOptions::default()
        .with_agent_change(&change)
        .unwrap();
    assert_eq!(
        options.agent.unwrap(),
        object(r#"{"model":{"provider":"faux","modelId":"faux-1"}}"#)
    );
    let created = ConversationCreateOptions::new(ConversationOwnership::Ownerless)
        .with_agent_change(&change)
        .unwrap();
    assert!(created.agent.is_some());
}

#[test]
fn extra_fields_of_a_model_change_persist_as_upstream_spreads_them() {
    let wire =
        object(r#"{"model":{"provider":"p","modelId":"m","label":"Fast","limits":{"k":[1,2]}}}"#);
    let typed = AgentChange::try_from(&wire).unwrap();
    let Change::Set(model) = &typed.model else {
        panic!("model was not set");
    };
    assert_eq!(model.model.provider.to_string_lossy(), "p");
    assert_eq!(model.extra["label"], serde_json::json!("Fast"));
    assert_eq!(JsonObject::try_from(&typed).unwrap(), wire);
    let mut stored = AgentState::default();
    apply_change(&mut stored, &JsonObject::try_from(&typed).unwrap()).unwrap();
    assert_eq!(
        stored.0,
        object(r#"{"model":{"provider":"p","modelId":"m","label":"Fast","limits":{"k":[1,2]}}}"#)
    );
}
