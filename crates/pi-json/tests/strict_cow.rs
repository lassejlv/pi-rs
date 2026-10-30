use pi_json::strict::{Value, parse};

#[test]
fn clones_share_containers_until_the_changed_path_is_written() {
    let before = parse(r#"{"items":[1,2],"nested":{"count":0},"other":{"label":"keep"}}"#).unwrap();
    let mut after = before.clone();
    assert!(std::ptr::eq(
        before.as_object().unwrap(),
        after.as_object().unwrap()
    ));
    after
        .as_object_mut()
        .unwrap()
        .get_mut(&pi_json::strict::JsonString::from("nested"))
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("count".into(), Value::from(1usize));
    assert_eq!(before["nested"]["count"].as_f64(), Some(0.0));
    assert_eq!(after["nested"]["count"].as_f64(), Some(1.0));
    assert!(std::ptr::eq(
        before["items"].as_array().unwrap(),
        after["items"].as_array().unwrap()
    ));
    assert!(std::ptr::eq(
        before["other"].as_object().unwrap(),
        after["other"].as_object().unwrap()
    ));
    assert!(!std::ptr::eq(
        before["nested"].as_object().unwrap(),
        after["nested"].as_object().unwrap()
    ));
}

#[test]
fn mutating_a_shared_array_keeps_its_previous_revision_and_untouched_children() {
    let before = parse(r#"[{"value":1},{"value":2}]"#).unwrap();
    let mut after = before.clone();
    assert!(std::ptr::eq(
        before.as_array().unwrap(),
        after.as_array().unwrap()
    ));
    after.as_array_mut().unwrap()[0]
        .as_object_mut()
        .unwrap()
        .insert("value".into(), Value::from(3usize));
    assert_eq!(before[0]["value"].as_f64(), Some(1.0));
    assert_eq!(after[0]["value"].as_f64(), Some(3.0));
    assert!(std::ptr::eq(
        before[1].as_object().unwrap(),
        after[1].as_object().unwrap()
    ));
}
