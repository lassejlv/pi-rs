use pi_durable::{harness::tool::*, *};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::Arc;
fn decode<T: DeserializeOwned>(value: &JsonValue) -> T {
    serde_json::from_str(&value.to_string()).unwrap()
}
fn encode(value: &impl Serialize) -> JsonValue {
    pi_chord::json::to_value(value).unwrap()
}

#[tokio::test]
async fn receipts_and_usage_match_independent_source_writer_traces() {
    let fixture: JsonValue =
        serde_json::from_str(include_str!("fixtures/tool-results.json")).unwrap();
    assert!(fixture["reference"].string_equals("5e63241d0294ed3fcfc4388c737a8ea27e09c74e"));
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 16);
    for case in fixture["cases"].as_array().unwrap() {
        let session = Session::new(
            Arc::new(MemoryStorage::default()),
            SessionOptions {
                now: Arc::new(|| 42.0),
                document_retention_ms: decode(&case["retention"]),
                ..Default::default()
            },
        );
        let conversation = session
            .commit(
                |tx| async move { Ok(tx.create_root_conversation().await?.id) },
                &Context::default(),
            )
            .await
            .unwrap();
        let call = decode::<ToolCall>(&case["call"]);
        let result = decode::<ToolExecutionResult>(&case["result"]);
        let timestamp = decode(&case["meta"]["timestamp"]);
        let duration = decode(&case["meta"]["durationMs"]);
        let entry = session
            .commit(
                move |tx| async move {
                    append_tool_result(&tx, conversation, &call, &result, timestamp, duration).await
                },
                &Context::default(),
            )
            .await
            .unwrap();
        assert_eq!(encode(&entry), case["entry"], "{}", case["name"]);
        assert_eq!(
            encode(
                &session
                    .snapshot(&USAGE_DOC, conversation, &Context::default())
                    .await
                    .unwrap()
            ),
            case["usage"],
            "{}",
            case["name"]
        );
        session.close(&Context::default()).await.unwrap();
    }
}

#[test]
fn nested_records_keep_explicit_null_utf16_metadata_and_legacy_receipts() {
    for source in [
        r#"{"taskId":7,"result":{"taskId":8,"structuredOutput":null,"isError":false,"details":{"\ud800":"\udfff"},"diagnostics":[{"severity":"error","message":"\ud800"}],"durationMs":0.5}}"#,
        r#"{"taskId":7}"#,
        r#"{}"#,
    ] {
        let raw = pi_chord::json::parse(source).unwrap();
        assert_eq!(encode(&decode::<NestedCallState>(&raw)), raw);
    }
    for source in [
        r#"{"entryId":7,"control":{"terminate":false,"handoff":"\ud800"}}"#,
        r#"{"kind":"nested"}"#,
        r#"{"kind":"model","entryId":7}"#,
    ] {
        let raw = pi_chord::json::parse(source).unwrap();
        assert_eq!(encode(&decode::<ToolTaskResult>(&raw)), raw);
    }
    let raw = pi_chord::json::parse(r#"{"kind":"nested","parent":7,"parentCallId":"\ud800","key":"\udfff","call":{"type":"toolCall","id":"\ud800/\udfff","name":"read","arguments":{"x":null},"argumentsError":"invalid","thoughtSignature":"\ud800"},"progress":false}"#).unwrap();
    assert_eq!(encode(&decode::<ToolTaskInput>(&raw)), raw);
    let raw = pi_chord::json::parse(
        r#"{"phase":"execute","arguments":{"\ud800":"\udfff"},"replay":"safe"}"#,
    )
    .unwrap();
    assert_eq!(encode(&decode::<ToolTaskCheckpoint>(&raw)), raw);
}

#[test]
fn streaming_leaf_assignments_replay_the_independent_source_changes() {
    let fixture: JsonValue =
        serde_json::from_str(include_str!("fixtures/tool-results.json")).unwrap();
    let cases = fixture["assignments"].as_array().unwrap();
    assert_eq!(cases.len(), 10);
    for case in cases {
        let mut value = case["current"].clone();
        let ops = pi_durable::harness::json::assign_json(
            &mut value,
            "value".into(),
            case["value"].clone(),
        )
        .unwrap();
        assert_eq!(value, case["after"], "{}", case["name"]);
        let source = case["ops"]
            .as_array()
            .unwrap()
            .iter()
            .map(|op| pi_chord::delta::Op::from_value(op).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            pi_chord::delta::apply(case["current"].clone(), &source).unwrap(),
            value
        );
        if case["name"].string_equals("identical records preserve noops") {
            assert!(ops.is_empty());
        }
        if case["name"].string_equals("grow array without rewriting existing objects") {
            assert!(
                ops.iter()
                    .any(|op| matches!(op,pi_chord::delta::Op::Append(path,_) if path.len()==3))
            );
            assert!(
                !ops.iter()
                    .any(|op| matches!(op,pi_chord::delta::Op::Set(path,_) if path.len()==1))
            );
        }
    }
}
