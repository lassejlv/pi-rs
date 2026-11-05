use pi_durable::{
    harness::{compaction::*, settings::resolve_settings},
    *,
};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::Arc;
fn decode<T: DeserializeOwned>(value: &JsonValue) -> T {
    serde_json::from_str(&value.to_string()).unwrap()
}
fn encode(value: &impl Serialize) -> JsonValue {
    pi_chord::json::to_value(value).unwrap()
}
#[test]
fn selection_sizes_serialization_and_policy_match_independent_source() {
    let fixture: JsonValue =
        serde_json::from_str(include_str!("fixtures/compaction.json")).unwrap();
    assert!(fixture["reference"].string_equals("5e63241d0294ed3fcfc4388c737a8ea27e09c74e"));
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 10);
    for case in fixture["cases"].as_array().unwrap() {
        let raw = &case["view"];
        let mut view = ContextView {
            head: decode(&raw["head"]),
            entries: decode(&raw["entries"]),
            contributions: decode(&raw["contributions"]),
            messages: decode(&raw["messages"]),
        };
        let aliases: Vec<Option<(usize, usize)>> = decode(&case["aliases"]);
        for (index, alias) in aliases.into_iter().enumerate() {
            if let Some((entry, message)) = alias {
                view.messages[index] = view.contributions[entry][message].clone();
            }
        }
        for cut in case["cuts"].as_array().unwrap() {
            assert_eq!(
                encode(&select_cut(&view, decode(&cut["budget"]))),
                cut["cut"],
                "{} {}",
                case["name"],
                cut["budget"]
            );
        }
        assert_eq!(
            estimate_context(&view, &decode::<Vec<Arc<JsonValue>>>(&case["extra"])),
            decode::<f64>(&case["tokens"]),
            "{}",
            case["name"]
        );
        assert_eq!(
            encode(
                &view
                    .contributions
                    .iter()
                    .map(|messages| messages
                        .iter()
                        .map(|message| estimate_message_tokens(message))
                        .collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            ),
            case["messageTokens"],
            "{}",
            case["name"]
        );
        assert_eq!(
            encode(&serialize_conversation(&view.messages)),
            case["serialized"],
            "{}",
            case["name"]
        );
        for (cut, expected) in case["prefixes"].as_array().unwrap().iter().enumerate() {
            assert_eq!(
                encode(&summarized_messages(&view, cut)),
                *expected,
                "{} prefix {cut}",
                case["name"]
            );
        }
    }
    for policy in fixture["policies"].as_array().unwrap() {
        let mut settings = resolve_settings(Default::default());
        settings.compaction = decode(&policy["compaction"]);
        assert_eq!(
            encode(&compaction_policy(&settings, &decode(&policy["model"])).unwrap()),
            policy["expected"]
        );
    }
}
