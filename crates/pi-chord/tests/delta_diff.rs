//! Ports of packages/chord/test/delta-diff.test.ts, named after the upstream cases.
#![allow(non_snake_case)]
use pi_chord::delta::*;
use pi_chord::json::{Map, Value, to_value};
use std::sync::Arc;
macro_rules! json { ($($tokens:tt)+) => { Value::from(serde_json::json!($($tokens)+)) }; }

fn ops(value: &Value) -> Vec<Op> {
    serde_json::from_str(&value.to_string()).unwrap()
}
fn expect_diff(before: &Value, after: &Value, expected: Value) {
    let operations = diff_revisions(before, after);
    assert_eq!(to_value(&operations).unwrap(), expected);
    assert_eq!(apply_immutable(Some(before), &operations).unwrap(), *after);
}
/// Arrays and objects that share child containers, as upstream shares JS references.
fn array(values: &[&Value]) -> Value {
    Value::Array(Arc::new(
        values.iter().map(|value| (*value).clone()).collect(),
    ))
}
fn record(pairs: &[(&str, &Value)]) -> Value {
    Value::Object(Arc::new(
        pairs
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).clone()))
            .collect::<Map>(),
    ))
}
fn rows(count: usize) -> Vec<serde_json::Value> {
    (0..count)
        .map(|value| serde_json::json!({ "value": value }))
        .collect()
}
fn object(ids: &[&str]) -> Vec<serde_json::Value> {
    ids.iter()
        .map(|id| serde_json::json!({ "id": id }))
        .collect()
}

#[test]
fn immutable_revision_diff__emits_sets_and_deletes() {
    expect_diff(
        &json!({"keep":1,"change":1,"remove":true}),
        &json!({"keep":1,"change":2,"add":3}),
        json!([["s", ["change"], 2], ["s", ["add"], 3], ["d", ["remove"]]]),
    );
}

#[test]
fn immutable_revision_diff__emits_string_append_and_front_truncation() {
    expect_diff(
        &json!({"text":"hello"}),
        &json!({"text":"hello world"}),
        json!([["a", ["text"], " world"]]),
    );
    expect_diff(
        &json!({"text":"hello world"}),
        &json!({"text":"world"}),
        json!([["t", ["text"], 6]]),
    );
    expect_diff(
        &json!({"text":"abcdefgh"}),
        &json!({"text":"defghxyz"}),
        json!([["t", ["text"], 3], ["a", ["text"], "xyz"]]),
    );
}

#[test]
fn immutable_revision_diff__represents_array_insertion_removal_and_shift_with_splices() {
    let [a, b, c] = [json!({"id":"a"}), json!({"id":"b"}), json!({"id":"c"})];
    expect_diff(
        &json!({"values":[a, b]}),
        &json!({"values":[a, c, b]}),
        json!([["p", ["values"], 1, 0, [c]]]),
    );
    expect_diff(
        &json!({"values":[a, b, c]}),
        &json!({"values":[b, c]}),
        json!([["p", ["values"], 0, 1, []]]),
    );
}

#[test]
fn immutable_revision_diff__collapses_a_same_length_queue_update_to_two_splices() {
    let [a, b, c, d] = [
        json!({"id":"a"}),
        json!({"id":"b"}),
        json!({"id":"c"}),
        json!({"id":"d"}),
    ];
    expect_diff(
        &json!({"values":[a, b, c]}),
        &json!({"values":[b, c, d]}),
        json!([["p", ["values"], 0, 1, []], ["p", ["values"], 2, 0, [d]]]),
    );
}

#[test]
fn immutable_revision_diff__emits_a_permutation_for_a_pure_reorder() {
    let [a, b, c] = [json!({"id":"a"}), json!({"id":"b"}), json!({"id":"c"})];
    expect_diff(
        &record(&[("values", &array(&[&a, &b, &c]))]),
        &record(&[("values", &array(&[&c, &a, &b]))]),
        json!([["m", ["values"], [2, 0, 1]]]),
    );
}

#[test]
fn immutable_revision_diff__normalizes_reordered_distinct_deeply_equal_objects_to_a_no_op() {
    // Two separately built equal objects: native values have no identity to tell them apart.
    let first = json!({"nested":{"value":1}});
    let second = json!({"nested":{"value":1}});
    assert!(
        diff_revisions(
            &json!({"values":[first, second]}),
            &json!({"values":[second, first]})
        )
        .is_empty()
    );
}

#[test]
fn immutable_revision_diff__validates_and_encodes_permutations() {
    let operations = ops(&json!([
        ["m", ["values"], [2, 0, 1]],
        ["m", ["values"], [1, 2, 0]]
    ]));
    for operation in &operations {
        operation.validate().unwrap();
    }
    let wire = Encoder::new().encode(&operations).unwrap();
    assert_eq!(
        wire,
        vec![json!(["m", ["values"], [2, 0, 1]]), json!(["m", [1, 2, 0]])]
    );
    for operation in &wire {
        assert_valid_wire_op(operation).unwrap();
    }
    assert_eq!(Decoder::new().decode(&wire).unwrap(), operations);
    assert!(
        Op::from_value(&json!(["m", ["values"], [0, 0]]))
            .unwrap_err()
            .to_string()
            .contains("bijection")
    );
}

#[test]
fn immutable_revision_diff__emits_nothing_for_deeply_equal_reconstructed_values() {
    assert!(
        diff_revisions(
            &json!({"value":{"nested":[1,2]}}),
            &json!({"value":{"nested":[1,2]}})
        )
        .is_empty()
    );
    assert!(
        diff_revisions(
            &json!({"values":[{"id":1},{"id":2}]}),
            &json!({"values":[{"id":1},{"id":2}]})
        )
        .is_empty()
    );
    assert!(
        diff_revisions(
            &json!({"values":[true,true,true]}),
            &json!({"values":[true,true,true]})
        )
        .is_empty()
    );
}

#[test]
fn immutable_revision_diff__keeps_a_leaf_edit_inside_a_reconstructed_array_narrow() {
    expect_diff(
        &json!({"values":[{"id":1,"label":"one"},{"id":2,"label":"two"}]}),
        &json!({"values":[{"id":1,"label":"one"},{"id":2,"label":"changed"}]}),
        json!([["s", ["values", 1, "label"], "changed"]]),
    );
}

#[test]
fn immutable_revision_diff__emits_payload_free_splices_for_scattered_removals() {
    let v = object(&["a", "b", "c", "d", "e"]);
    let kept = vec![v[0].clone(), v[2].clone(), v[4].clone()];
    expect_diff(
        &json!({"values":v}),
        &json!({"values":kept}),
        json!([["p", ["values"], 1, 1, []], ["p", ["values"], 2, 1, []]]),
    );
}

#[test]
fn immutable_revision_diff__encodes_removal_canonically() {
    let cases: [(&str, Vec<i32>, Vec<i32>, serde_json::Value); 5] = [
        (
            "front",
            vec![1, 2, 3, 4],
            vec![2, 3, 4],
            serde_json::json!([["p", ["values"], 0, 1, []]]),
        ),
        (
            "tail",
            vec![1, 2, 3, 4],
            vec![1, 2, 3],
            serde_json::json!([["p", ["values"], 3, 1, []]]),
        ),
        (
            "middle",
            vec![1, 2, 3, 4],
            vec![1, 3, 4],
            serde_json::json!([["p", ["values"], 1, 1, []]]),
        ),
        (
            "all",
            vec![1, 2, 3, 4],
            vec![],
            serde_json::json!([["p", ["values"], 0, 4, []]]),
        ),
        (
            "none",
            vec![1, 2, 3, 4],
            vec![1, 2, 3, 4],
            serde_json::json!([]),
        ),
    ];
    for (name, before, after, expected) in cases {
        expect_diff(
            &json!({"values":before}),
            &json!({"values":after}),
            Value::from(expected),
        );
        let _ = name;
    }
}

#[test]
fn immutable_revision_diff__does_not_field_diff_unrelated_shifted_objects_with_common_fields() {
    let make = |values: [i32; 3]| -> Vec<_> {
        values
            .iter()
            .map(|v| serde_json::json!({"type":"row","value":v}))
            .collect()
    };
    expect_diff(
        &json!({"values":make([1, 2, 3])}),
        &json!({"values":make([2, 3, 4])}),
        json!([["p", ["values"], 0, 1, []], ["p", ["values"], 2, 0, [{"type":"row","value":4}]]]),
    );
}

#[test]
fn immutable_revision_diff__does_not_treat_coincidental_id_or_key_fields_as_structural_identity() {
    let left = serde_json::json!({"value":"left"});
    let right = serde_json::json!({"value":"right"});
    let before = serde_json::json!([
        left, {"id":1,"key":"a","value":"first"}, {"id":2,"key":"b","value":"second"}, right
    ]);
    let replacements = serde_json::json!([
        {"id":2,"key":"b","value":"edited-second"}, {"id":1,"key":"a","value":"edited-first"}
    ]);
    let after = serde_json::json!([left, replacements[0], replacements[1], right]);
    expect_diff(
        &json!({"values":before}),
        &json!({"values":after}),
        json!([["p", ["values"], 1, 2, replacements]]),
    );
}

#[test]
fn immutable_revision_diff__combines_removals_append_and_a_shared_subtree_survivor_edit() {
    // `stable: c.stable` shares the survivor's subtree by identity (`Arc`), which aligns
    // the edited `c` with its original rather than splicing it.
    let mk = |id: &str| json!({"id":id,"stable":{},"detail":{"text":id}});
    let (a, b, c, d) = (mk("a"), mk("b"), mk("c"), mk("d"));
    let changed_c = record(&[
        ("id", &json!("c")),
        ("stable", &c["stable"]),
        ("detail", &json!({"text":"changed"})),
    ]);
    let appended = mk("e");
    expect_diff(
        &record(&[("values", &array(&[&a, &b, &c, &d]))]),
        &record(&[("values", &array(&[&a, &changed_c, &d, &appended]))]),
        json!([
            ["p", ["values"], 1, 1, []],
            ["a", ["values", 1, "detail", "text"], "hanged"],
            ["p", ["values"], 3, 0, [{"id":"e","stable":{},"detail":{"text":"e"}}]]
        ]),
    );
}

#[test]
fn immutable_revision_diff__does_not_retain_a_removed_neighbor_payload() {
    for size in [256 * 1024, 1024 * 1024] {
        let payload = "x".repeat(size);
        let retained: Vec<_> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|id| serde_json::json!({"id":id,"payload":payload}))
            .collect();
        let before = json!({"values":retained});
        let after = json!({"values":[retained[0], retained[2], retained[4]]});
        let operations = diff_revisions(&before, &after);
        assert_eq!(
            to_value(&operations).unwrap(),
            json!([["p", ["values"], 1, 1, []], ["p", ["values"], 2, 1, []]]),
            "{size}"
        );
        assert!(
            to_value(&operations).unwrap().to_string().len() < 100,
            "{size}"
        );
        assert_eq!(apply_immutable(Some(&before), &operations).unwrap(), after);
        assert_eq!(before["values"].as_array().unwrap().len(), 5);
    }
}

#[test]
fn immutable_revision_diff__keeps_push_pop_and_middle_removal_narrow() {
    for size in [1_001usize, 10_000] {
        let values = rows(size);
        let appended = serde_json::json!({"value":size});
        let mut pushed = values.clone();
        pushed.push(appended.clone());
        expect_diff(
            &json!({"values":values}),
            &json!({"values":pushed}),
            json!([["p", ["values"], size, 0, [appended]]]),
        );
        expect_diff(
            &json!({"values":values}),
            &json!({"values":values[..size - 1]}),
            json!([["p", ["values"], size - 1, 1, []]]),
        );
        let middle = size / 2;
        let mut removed = values.clone();
        removed.remove(middle);
        expect_diff(
            &json!({"values":values}),
            &json!({"values":removed}),
            json!([["p", ["values"], middle, 1, []]]),
        );
    }
}

#[test]
fn immutable_revision_diff__keeps_forty_thousand_row_sparse_edits_narrow() {
    let values: Vec<_> = (0..40_000)
        .map(|value| serde_json::json!({"value":value,"stable":{"value":value}}))
        .collect();
    let mut after = values.clone();
    let mut expected = Vec::new();
    for index in (100..after.len()).step_by(400) {
        after[index] =
            serde_json::json!({"value": -(index as i64), "stable": values[index]["stable"]});
        expected.push(serde_json::json!([
            "s",
            ["values", index, "value"],
            -(index as i64)
        ]));
    }
    let operations = diff_revisions(&json!({"values":values}), &json!({"values":after}));
    let wire = to_value(&operations).unwrap();
    assert_eq!(wire, Value::from(serde_json::Value::Array(expected)));
    assert!(wire.to_string().len() < 7_500);
    assert_eq!(
        apply_immutable(Some(&json!({"values":values})), &operations).unwrap(),
        json!({"values":after})
    );
}

#[test]
fn immutable_revision_diff__keeps_a_reconstructed_large_array_leaf_edit_narrow() {
    let before: Vec<_> = (0..1_000)
        .map(|value| serde_json::json!({"value":value,"label":format!("row-{value}")}))
        .collect();
    let mut after = before.clone();
    after[700]["label"] = "changed".into();
    let operations = diff_revisions(&json!({"values":before}), &json!({"values":after}));
    let wire = to_value(&operations).unwrap();
    assert_eq!(wire, json!([["s", ["values", 700, "label"], "changed"]]));
    assert!(wire.to_string().len() < 100);
    assert_eq!(
        apply_immutable(Some(&json!({"values":before})), &operations).unwrap(),
        json!({"values":after})
    );
}

#[test]
fn immutable_revision_diff__splices_an_ambiguous_equal_count_moved_and_edited_gap() {
    let left = serde_json::json!({"value":"left"});
    let right = serde_json::json!({"value":"right"});
    let before = serde_json::json!([left, {"id":1,"value":"a"}, {"id":2,"value":"b"}, right]);
    let replacements =
        serde_json::json!([{"id":2,"value":"edited"}, {"id":1,"value":"also-edited"}]);
    let after = serde_json::json!([left, replacements[0], replacements[1], right]);
    expect_diff(
        &json!({"values":before}),
        &json!({"values":after}),
        json!([["p", ["values"], 1, 2, replacements]]),
    );
}

#[test]
fn immutable_revision_diff__keeps_a_large_rotation_payload_free() {
    let values: Vec<Value> = rows(10_000).into_iter().map(Value::from).collect();
    let mut rotated = values[1_000..].to_vec();
    rotated.extend_from_slice(&values[..1_000]);
    let before = record(&[("values", &Value::Array(Arc::new(values)))]);
    let after = record(&[("values", &Value::Array(Arc::new(rotated)))]);
    let operations = diff_revisions(&before, &after);
    assert_eq!(operations.len(), 1);
    assert!(matches!(operations[0], Op::Move(..)));
    assert!(to_value(&operations).unwrap().to_string().len() < 60_000);
    assert_eq!(apply_immutable(Some(&before), &operations).unwrap(), after);
}

#[test]
fn immutable_revision_diff__encodes_five_hundred_unshifts_without_snapshotting_retained_rows() {
    let retained: Vec<_> = (0..10_000)
        .map(|value| serde_json::json!({"value":value,"payload":"x".repeat(100)}))
        .collect();
    let inserted: Vec<_> = (0..500)
        .map(|value| serde_json::json!({"value": -value - 1}))
        .collect();
    let mut all = inserted.clone();
    all.extend(retained.iter().cloned());
    let operations = diff_revisions(&json!({"values":retained}), &json!({"values":all}));
    let wire = to_value(&operations).unwrap();
    assert_eq!(wire, json!([["p", ["values"], 0, 0, inserted]]));
    assert!(wire.to_string().len() < 20_000);
    assert_eq!(
        apply_immutable(Some(&json!({"values":retained})), &operations).unwrap(),
        json!({"values":all})
    );
}

#[test]
fn immutable_revision_diff__bounds_wide_object_operation_emission_with_a_root_replacement() {
    let build = |value: i32| {
        Value::from(serde_json::Value::Object(
            (0..20_000)
                .map(|index| (format!("field{index}"), value.into()))
                .collect(),
        ))
    };
    let (before, after) = (build(0), build(1));
    assert_eq!(diff_revisions(&before, &after), vec![Op::Replace(after)]);
}

#[test]
fn immutable_revision_diff__bounds_a_wide_normalized_array_fallback_with_a_root_replacement() {
    let before = json!({"values": vec![0; 40_000]});
    let after = json!({"values": vec![1; 40_000]});
    let operations = diff_revisions(&before, &after);
    assert_eq!(operations, vec![Op::Replace(after.clone())]);
    assert_eq!(apply_immutable(Some(&before), &operations).unwrap(), after);
}
