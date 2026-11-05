use pi_durable::{codemode::describe::*, harness::declaration::ToolDeclaration, *};
#[test]
fn catalog_and_samples_preserve_independent_utf16_declarations() {
    let fixture = pi_chord::json::parse(include_str!(
        "../../pi-codemode/tests/fixtures/declarations-reference.json"
    ))
    .unwrap();
    for (i, row) in fixture["rows"].as_array().unwrap().iter().enumerate() {
        let tool = &row["tool"];
        let name = tool["name"].as_string().unwrap().clone();
        let metadata = ToolMetadata {
            declaration: ToolDeclaration {
                name: name.clone(),
                description: tool["description"].as_string().unwrap().clone(),
                parameters: tool["inputSchema"].clone(),
                constrained_sampling: None,
            },
            structured_output_schema: Some(tool["outputSchema"].clone()),
        };
        let sample = sample_of(&metadata).unwrap();
        assert_eq!(sample, *row["sample"].as_string().unwrap(), "sample {i}");
        let mut expected =
            JsonString::from("Tools scripts can call (`await tools.<name>(args)`):\n\n### `");
        expected.append(&JsonString::from_utf16(
            row["signature"]
                .as_string()
                .unwrap()
                .units()
                .iter()
                .copied()
                .take_while(|u| *u != 40)
                .collect(),
        ));
        expected.append(&"` (`".into());
        expected.append(&name);
        expected.append(&"`)\n".into());
        expected.append(&sample);
        assert_eq!(
            codemode_section(&[metadata], &[], f64::INFINITY)
                .unwrap()
                .unwrap(),
            expected,
            "catalog {i}"
        );
    }
}
