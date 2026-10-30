use pi_durable::{
    harness::{declaration::ToolDeclaration, registry::ToolCaller, tool::ReplayPolicy},
    search::{SearchDocument, rank_documents, tokenize},
    tool_search::*,
    *,
};

#[test]
fn search_preserves_token_rules_stable_ties_and_numeric_limits() {
    assert_eq!(
        tokenize(&"listIssues for the GitHub_repo searches queries HTTPServer".into()),
        [
            "list", "issue", "git", "hub", "repo", "search", "query", "http", "server"
        ]
    );
    assert_eq!(
        tokenize(&JsonString::from_utf16(vec![
            65, 66, 99, 0xd800, 0x212a, 0x130
        ])),
        ["bc", "ki"]
    );
    let docs = vec![
        SearchDocument {
            name: "first".into(),
            text: "issue".into(),
        },
        SearchDocument {
            name: JsonString::from_utf16(vec![0xd800]),
            text: "issue".into(),
        },
        SearchDocument {
            name: "long".into(),
            text: "issue file repository server image".into(),
        },
    ];
    let names: Vec<_> = docs.iter().map(|d| d.name.clone()).collect();
    assert_eq!(rank_documents(&"issues issue".into(), &docs, 8.0), names);
    assert_eq!(rank_documents(&"issue".into(), &docs, 1.99), names[..1]);
    for limit in [0.0, -1.0, f64::NAN, 0.99] {
        assert!(rank_documents(&"issue".into(), &docs, limit).is_empty());
    }
    assert_eq!(rank_documents(&"issue".into(), &docs, f64::INFINITY), names);
    assert!(rank_documents(&"the and".into(), &docs, 8.0).is_empty());
}

#[test]
fn tool_search_traverses_schema_descriptions_and_preserves_lossless_loads() {
    let name = JsonString::from_utf16(vec![0xd800]);
    let candidates = vec![ToolDeclaration::new(name.clone(), "\u{feff} First line\r\nsecond line ", pi_chord::json::parse(r#"{"properties":{"entries":{"items":{"anyOf":[{"description":"Archived repositories"}]}}}}"#).unwrap())];
    let result = execute_search(
        ToolSearchToolInput {
            query: "weather".into(),
            limit: None,
        },
        &candidates,
    )
    .unwrap();
    assert!(result.control.is_none());
    let result = execute_search(
        ToolSearchToolInput {
            query: "repositories".into(),
            limit: None,
        },
        &candidates,
    )
    .unwrap();
    assert_eq!(
        result.control.as_ref().unwrap().add_tools.as_ref().unwrap(),
        std::slice::from_ref(&name)
    );
    let mut text = JsonString::from("Loaded 1 tool. They are available from your next call:\n- ");
    text.append(&name);
    text.append(&": First line".into());
    assert_eq!(
        result.output.unwrap()[0][&JsonString::from("text")],
        JsonValue::String(text)
    );
    let tool = create_tool_search_tool();
    assert_eq!(tool.callers, Some(vec![ToolCaller::Model]));
    assert_eq!(tool.replay, ReplayPolicy::Safe);
    for limit in [0.0, -1.0, 0.5, f64::INFINITY, f64::NAN] {
        assert_eq!(
            execute_search(
                ToolSearchToolInput {
                    query: "x".into(),
                    limit: Some(limit)
                },
                &[]
            )
            .unwrap_err()
            .to_string(),
            "limit must be a positive integer"
        );
    }
    assert_eq!(
        execute_search(
            ToolSearchToolInput {
                query: "\u{feff}\u{a0}".into(),
                limit: Some(-1.0)
            },
            &[]
        )
        .unwrap_err()
        .to_string(),
        "query must not be empty"
    );
}
