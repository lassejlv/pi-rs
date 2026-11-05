//! JSON-lines test transport for the unchanged upstream declaration tests.
use pi_codemode::declarations::json::*;
use pi_json::strict::{self, JsonString, Map, Value};
use std::io::{BufRead, Write};
fn declaration(value: &Value) -> JsonToolDeclaration {
    let object = value.as_object().expect("tool declaration");
    JsonToolDeclaration {
        name: value["name"].as_string().expect("tool name").clone(),
        description: value["description"].as_string().cloned(),
        input_schema: object.get(&JsonString::from("inputSchema")).cloned(),
        output_schema: object.get(&JsonString::from("outputSchema")).cloned(),
        signature: value["signature"].as_string().cloned(),
    }
}
fn declarations(value: &Value) -> Vec<JsonToolDeclaration> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(declaration)
        .collect()
}
fn main() {
    let mut calls = 0usize;
    let mut output = std::io::BufWriter::new(std::io::stdout().lock());
    for line in std::io::stdin().lock().lines() {
        let request = strict::parse(&line.unwrap()).unwrap();
        let method = request["method"].to_utf8().expect("method");
        let budget = request["maxChars"].as_u64().map(|v| v as usize);
        let value = match method.as_str() {
            "transport.ready" => Value::Bool(true),
            "stats" => Value::from(calls),
            "schemaToType" => {
                calls += 1;
                Value::String(schema_to_json_type(&request["schema"], budget))
            }
            "renderToolSignature" => {
                calls += 1;
                Value::String(render_json_tool_signature(
                    &declaration(&request["tool"]),
                    budget,
                ))
            }
            "renderToolSample" => {
                calls += 1;
                Value::String(render_json_tool_sample(
                    &declaration(&request["tool"]),
                    budget,
                ))
            }
            "renderDeclarations" => {
                calls += 1;
                Value::String(render_json_declarations(
                    &declarations(&request["tools"]),
                    &declarations(&request["globals"]),
                ))
            }
            _ => panic!("Unknown declaration test operation: {method}"),
        };
        let response = Value::Object(
            Map::from_iter([
                ("id".into(), request["id"].clone()),
                ("value".into(), value),
            ])
            .into(),
        );
        writeln!(output, "{}", strict::stringify(&response)).unwrap();
        output.flush().unwrap();
    }
}
