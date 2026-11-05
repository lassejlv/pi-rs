use pi_durable::{codemode::output::*, *};
use std::sync::{Arc, Mutex};
fn decode<T: serde::de::DeserializeOwned>(value: &JsonValue) -> T {
    serde_json::from_str(&value.to_string()).unwrap()
}
fn encode(value: &impl serde::Serialize) -> JsonValue {
    pi_chord::json::to_value(value).unwrap()
}
#[tokio::test]
async fn script_formatting_matches_independent_reference_including_lossless_spills_and_image_deduplication()
 {
    let fixture =
        pi_chord::json::parse(include_str!("fixtures/codemode-output-reference.json")).unwrap();
    for (index, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
        let writes = Arc::new(Mutex::new(Vec::<JsonValue>::new()));
        let capture = writes.clone();
        let fail = case["fail"].as_bool().unwrap();
        let write: OutputFile = Arc::new(move |name, data| {
            let mut record = pi_chord::json::Map::new();
            record.insert("name".into(), encode(&name));
            match data {
                OutputData::Text(text) => {
                    record.insert("text".into(), encode(&text));
                }
                OutputData::Binary(bytes) => {
                    record.insert("bytes".into(), encode(&bytes));
                }
            }
            capture
                .lock()
                .unwrap()
                .push(JsonValue::Object(record.into()));
            Box::pin(async move {
                Ok(if fail {
                    OutputFileResult::Failed(JsonString::from_utf16(
                        "write failed".encode_utf16().chain([0xd800]).collect(),
                    ))
                } else {
                    OutputFileResult::Saved(format!("/output/{name}").into())
                })
            })
        });
        let options = &case["options"];
        let formatted = format_result(
            case["result"].clone(),
            FormatResultOptions {
                generated_images: decode(&options["generatedImages"]),
                wall_ms: decode(&options["wallMs"]),
                max_output_tokens: decode(&options["maxOutputTokens"]),
                calls: decode(&options["calls"]),
                max_bytes: decode(&options["limits"]["maxBytes"]),
                max_lines: decode(&options["limits"]["maxLines"]),
                write,
            },
        )
        .await
        .unwrap();
        let mut actual = pi_chord::json::Map::new();
        actual.insert("content".into(), encode(&formatted.content));
        if let Some(path) = formatted.full_output_path {
            actual.insert("fullOutputPath".into(), encode(&path));
        }
        assert_eq!(
            JsonValue::Object(actual.into()),
            case["expected"],
            "result case {index}"
        );
        assert_eq!(
            encode(&*writes.lock().unwrap()),
            case["writes"],
            "writes case {index}"
        );
    }
}
