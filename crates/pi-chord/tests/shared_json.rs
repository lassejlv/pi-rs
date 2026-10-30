use pi_chord::{
    JsonValue,
    delta::{Op, SharedJson},
};
fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
fn ops(value: &JsonValue) -> Vec<Op> {
    serde_json::from_str(&value.to_string()).unwrap()
}
#[test]
fn every_pinned_application_stream_and_rejection_matches_the_source() {
    let fixture = json(include_str!("fixtures/delta.json"));
    let mut value = SharedJson::new(JsonValue::Null);
    for row in fixture["streams"].as_array().unwrap() {
        value = value.apply(&ops(&row["ops"])).unwrap();
        assert_eq!(value.to_value(), row["value"]);
    }
    for row in fixture["application"].as_array().unwrap() {
        assert_eq!(
            SharedJson::new(row["initial"].clone())
                .apply(&ops(&row["ops"]))
                .unwrap()
                .to_value(),
            row["value"]
        );
    }
    for row in fixture["applicationErrors"].as_array().unwrap() {
        assert_eq!(
            SharedJson::new(row["initial"].clone())
                .apply(&ops(&row["ops"]))
                .unwrap_err()
                .to_string(),
            row["message"].to_utf8().unwrap()
        );
    }
}
#[test]
fn edits_copy_ancestors_while_moves_and_splices_retain_unmodified_children() {
    let initial = SharedJson::new(json(
        r#"{"rows":[{"details":{"n":1}},{"details":{"n":2}}],"other":{}}"#,
    ));
    let moved = initial
        .apply(&ops(&json(
            r#"[["m",["rows"],[1,0]],["p",["rows"],0,1,[]],["s",["rows",0,"status"],"running"]]"#,
        )))
        .unwrap();
    assert!(
        initial
            .get_key("other")
            .unwrap()
            .ptr_eq(moved.get_key("other").unwrap())
    );
    assert!(
        initial
            .get(&["rows".into(), 0.into(), "details".into()])
            .unwrap()
            .ptr_eq(
                moved
                    .get(&["rows".into(), 0.into(), "details".into()])
                    .unwrap()
            )
    );
    assert!(
        !initial
            .get(&["rows".into(), 0.into()])
            .unwrap()
            .ptr_eq(moved.get(&["rows".into(), 0.into()]).unwrap())
    );
    assert_eq!(
        initial.to_value(),
        json(r#"{"rows":[{"details":{"n":1}},{"details":{"n":2}}],"other":{}}"#)
    );
    assert!(initial.ptr_eq(&initial.apply(&[]).unwrap()));
}
#[test]
fn equal_container_replacements_have_new_identity_and_primitives_compare_by_value() {
    let a = SharedJson::new(json(r#"{"object":{},"text":"same"}"#));
    let b = a
        .apply(&ops(&json(
            r#"[["s",["object"],{}],["s",["text"],"same"]]"#,
        )))
        .unwrap();
    assert!(
        !a.get_key("object")
            .unwrap()
            .same_value(b.get_key("object").unwrap())
    );
    assert!(
        a.get_key("text")
            .unwrap()
            .same_value(b.get_key("text").unwrap())
    );
    let weak = b.downgrade();
    assert!(weak.upgrade().unwrap().ptr_eq(&b));
    drop(b);
    assert!(weak.upgrade().is_none());
}
