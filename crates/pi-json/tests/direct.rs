//! The direct serde bridge agrees with the text route and keeps lone surrogates lossless.
use pi_json::strict::{JsonString, Map, Value, from_value, parse, stringify, to_value};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Inner {
    count: u64,
    ratio: f64,
    label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Tagged {
    Unit,
    Pair { left: i64, right: Inner },
}
#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum External {
    Plain,
    Newtype(Vec<u8>),
    Tuple(i32, String),
    Struct { raw: Value },
}
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Everything {
    inner: Inner,
    tagged: Vec<Tagged>,
    external: Vec<External>,
    by_name: BTreeMap<String, i32>,
    raw: Value,
    key: JsonString,
    keyed: Map,
}

fn sample() -> Everything {
    let lone = JsonString::from_utf16(vec![0x61, 0xd800, 0x62, 0xdc00]);
    let mut keyed = Map::new();
    keyed.insert(lone.clone(), Value::String(lone.clone()));
    keyed.insert(JsonString::from("2"), Value::Bool(true));
    keyed.insert(JsonString::from("b"), Value::Null);
    Everything {
        inner: Inner {
            count: 3,
            ratio: 0.5,
            label: None,
            note: Some("n".into()),
        },
        tagged: vec![
            Tagged::Unit,
            Tagged::Pair {
                left: -4,
                right: Inner {
                    count: 0,
                    ratio: 2.0,
                    label: Some("l".into()),
                    note: None,
                },
            },
        ],
        external: vec![
            External::Plain,
            External::Newtype(vec![1, 2, 3]),
            External::Tuple(-1, "t".into()),
            External::Struct {
                raw: parse(r#"{"a":[1,2.5,"x"],"b":{"c":null}}"#).unwrap(),
            },
        ],
        by_name: BTreeMap::from([("one".into(), 1), ("two".into(), 2)]),
        raw: Value::String(lone.clone()),
        key: lone,
        keyed,
    }
}

#[test]
fn direct_encoding_matches_text_encoding() {
    let value = sample();
    let direct = to_value(&value).unwrap();
    let text = parse(
        &serde_json::to_string(&serde_json::json!({
            "inner": {"count": 3, "ratio": 0.5, "label": null, "note": "n"},
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(direct["inner"], text["inner"]);
    // Re-reading the encoded value gives back the original, including lone surrogates in strings and keys.
    let back: Everything = from_value(&direct).unwrap();
    assert_eq!(back, value);
    // And its text form parses back to the same value.
    assert_eq!(parse(&stringify(&direct)).unwrap(), direct);
}

#[test]
fn integral_floats_read_as_integers_like_their_text() {
    let number = Value::Number(serde_json::Number::from_f64(3.0).unwrap());
    assert_eq!(from_value::<u64>(&number).unwrap(), 3);
    assert_eq!(
        from_value::<i64>(&Value::Number(serde_json::Number::from_f64(-7.0).unwrap())).unwrap(),
        -7
    );
    assert_eq!(from_value::<f64>(&number).unwrap(), 3.0);
    assert!(from_value::<u8>(&Value::Number(300.into())).is_err());
}

#[test]
fn errors_fall_back_to_the_text_route() {
    let error = from_value::<Inner>(&parse(r#"{"count":"x","ratio":1}"#).unwrap()).unwrap_err();
    assert!(error.contains("invalid type"), "{error}");
}

#[test]
fn a_lone_surrogate_cannot_become_a_utf8_string_but_survives_as_a_value() {
    let lone = Value::String(JsonString::from_utf16(vec![0xd800]));
    assert!(from_value::<String>(&lone).is_err());
    assert_eq!(from_value::<Value>(&lone).unwrap(), lone);
    assert_eq!(to_value(&lone).unwrap(), lone);
}

#[test]
fn text_serializers_and_deserializers_still_work_for_values() {
    let value = parse(r#"{"k":[1,"two",{"three":3}]}"#).unwrap();
    let text = serde_json::to_string(&value).unwrap();
    assert_eq!(text, r#"{"k":[1,"two",{"three":3}]}"#);
    let back: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(back, value);
    let nested: Vec<Value> = serde_json::from_str(r#"[{"a":1},"s",null]"#).unwrap();
    assert_eq!(nested.len(), 3);
}
