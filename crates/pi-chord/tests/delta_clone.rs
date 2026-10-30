use pi_chord::JsonValue;
use pi_chord::delta::{Op, Tracker, apply, track};
use pi_chord::json::parse;
use std::sync::Arc;

fn value(s: &str) -> JsonValue {
    parse(s).unwrap()
}

// delta-clone.test.ts > tracker ownership > takes immutable ownership of the imported revision in O(1)
#[test]
fn takes_immutable_ownership_of_the_imported_revision_in_o1() {
    let input = Arc::new(value(
        r#"{"point":{"x":3,"y":7,"pressure":0.1},"rows":[{"values":[0,false,null,"text",{"n":1}]}]}"#,
    ));
    let tracker = Tracker::new(input.clone());
    assert!(Arc::ptr_eq(tracker.value(), &input));
    assert!(std::ptr::eq(
        input["point"].as_object().unwrap(),
        tracker.value()["point"].as_object().unwrap()
    ));
    let values = |v: &JsonValue| v["rows"].as_array().unwrap()[0]["values"].clone();
    let before = values(&input);
    let after = values(tracker.value());
    assert!(std::ptr::eq(
        before.as_array().unwrap(),
        after.as_array().unwrap()
    ));
    let from_value = track(value(r#"{"point":{"x":3}}"#));
    assert_eq!(from_value.revision(), 0);
}

// delta-clone.test.ts > tracker ownership > copies assigned and inserted values immediately
#[test]
fn copies_assigned_and_inserted_values_immediately() {
    let tracker = track(value(r#"{"rows":[]}"#));
    let mut assigned = value(r#"{"nested":{"value":1}}"#);
    let mut change = tracker.begin_change();
    change
        .apply_ops(&[Op::Splice(
            vec!["rows".into()],
            0,
            0,
            vec![assigned.clone()],
        )])
        .unwrap();
    assigned
        .as_object_mut()
        .unwrap()
        .get_mut(&pi_chord::JsonString::from("nested"))
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("value".into(), value("9"));
    assert_eq!(assigned["nested"]["value"], value("9"));
    let rows = change.current().unwrap()["rows"].clone();
    assert_eq!(rows.as_array().unwrap()[0]["nested"]["value"], value("1"));
    let prepared = change.prepare().unwrap();
    let rows = prepared.value()["rows"].clone();
    assert_eq!(rows.as_array().unwrap()[0]["nested"]["value"], value("1"));
    let replica = apply(prepared.base().as_ref().clone(), prepared.ops()).unwrap();
    assert_eq!(replica, **prepared.value());
}
