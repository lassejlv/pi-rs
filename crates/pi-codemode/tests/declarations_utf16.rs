use pi_codemode::declarations::json::*;
use pi_json::strict::{self, JsonString, Value};
fn decl(v: &Value) -> JsonToolDeclaration {
    JsonToolDeclaration {
        name: v["name"].as_string().unwrap().clone(),
        description: v["description"].as_string().cloned(),
        input_schema: v
            .as_object()
            .unwrap()
            .get(&JsonString::from("inputSchema"))
            .cloned(),
        output_schema: v
            .as_object()
            .unwrap()
            .get(&JsonString::from("outputSchema"))
            .cloned(),
        signature: v["signature"].as_string().cloned(),
    }
}
#[test]
fn compares_independent_source_utf16_rendering() {
    let fixture = strict::parse(include_str!("fixtures/declarations-reference.json")).unwrap();
    for (i, row) in fixture["rows"].as_array().unwrap().iter().enumerate() {
        for variant in row["types"].as_array().unwrap() {
            assert_eq!(
                schema_to_json_type(
                    &row["schema"],
                    variant["maxChars"].as_u64().map(|v| v as usize)
                ),
                *variant["expected"].as_string().unwrap(),
                "schema {i}, budget {}",
                variant["maxChars"]
            );
        }
        let tool = decl(&row["tool"]);
        let globals: Vec<_> = row["globals"]
            .as_array()
            .unwrap()
            .iter()
            .map(decl)
            .collect();
        assert_eq!(
            render_json_tool_signature(&tool, None),
            *row["signature"].as_string().unwrap(),
            "signature {i}"
        );
        assert_eq!(
            render_json_tool_sample(&tool, None),
            *row["sample"].as_string().unwrap(),
            "sample {i}"
        );
        assert_eq!(
            render_json_declarations(&[tool], &globals),
            *row["declarations"].as_string().unwrap(),
            "declarations {i}"
        );
    }
}
