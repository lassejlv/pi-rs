#![allow(non_snake_case)]
use pi_chord::delta::*;
use pi_chord::json::{Value, to_value};
macro_rules! json { ($($tokens:tt)+) => { Value::from(serde_json::json!($($tokens)+)) }; }

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/delta.json")).unwrap()
}
fn ops(value: &Value) -> Vec<Op> {
    serde_json::from_str(&value.to_string()).unwrap()
}

#[test]
fn pinned_reference_stream_encodes_decodes_and_replays() {
    let fixture = fixture();
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    let mut state = Value::Null;
    for (index, row) in fixture["streams"].as_array().unwrap().iter().enumerate() {
        let batch = ops(&row["ops"]);
        let wire = encoder.encode(&batch).unwrap();
        assert_eq!(
            to_value(&wire).unwrap(),
            row["wire"],
            "encode batch {index}"
        );
        let decoded = decoder.decode(row["wire"].as_array().unwrap()).unwrap();
        assert_eq!(decoded, batch, "decode batch {index}");
        state = apply(state, &decoded).unwrap();
        assert_eq!(state, row["value"], "apply batch {index}");
    }
}

#[test]
fn pinned_reference_application_and_input_ownership() {
    for row in fixture()["application"].as_array().unwrap() {
        let initial = row["initial"].clone();
        let operations = ops(&row["ops"]);
        assert_eq!(
            apply_immutable(Some(&initial), &operations).unwrap(),
            row["value"]
        );
        assert_eq!(initial, row["initial"]);
        assert_eq!(to_value(&operations).unwrap(), row["ops"]);
    }
}

#[test]
fn pinned_reference_validation_errors() {
    let fixture = fixture();
    for row in fixture["errors"].as_array().unwrap() {
        assert_eq!(
            Op::from_value(&row["op"]).unwrap_err().to_string(),
            row["message"].to_utf8().unwrap(),
            "{}",
            row["op"]
        );
    }
    for row in fixture["wireErrors"].as_array().unwrap() {
        assert_eq!(
            Decoder::new()
                .decode(row["wire"].as_array().unwrap())
                .unwrap_err()
                .to_string(),
            row["message"].to_utf8().unwrap(),
            "{}",
            row["wire"]
        );
    }
    for row in fixture["applicationErrors"].as_array().unwrap() {
        assert_eq!(
            apply(row["initial"].clone(), &ops(&row["ops"]))
                .unwrap_err()
                .to_string(),
            row["message"].to_utf8().unwrap(),
            "{}",
            row["ops"]
        );
    }
}

#[test]
fn pinned_reference_overlap_uses_utf16_units_and_candidate_budget() {
    for row in fixture()["overlaps"].as_array().unwrap() {
        let a = row["args"].as_array().unwrap();
        assert_eq!(
            overlap(
                &a[0].to_utf8().unwrap(),
                &a[1].to_utf8().unwrap(),
                a[2].as_u64().unwrap() as usize,
                a[3].as_u64().unwrap() as usize,
                a[4].as_u64().unwrap() as usize
            ),
            row["value"].as_u64().unwrap() as usize
        );
    }
}

#[test]
fn base_resets_dictionaries_and_each_batch_needs_an_explicit_first_path() {
    let mut enc = Encoder::new();
    let mut dec = Decoder::new();
    for i in 0..3 {
        let batch = ops(&json!([["s", ["a"], i]]));
        let wire = enc.encode(&batch).unwrap();
        assert_eq!(dec.decode(&wire).unwrap(), batch);
    }
    dec.decode(&enc.encode(&[Op::Replace(json!({"a":0}))]).unwrap())
        .unwrap();
    assert!(dec.decode(&[json!(["s", 0, 3])]).is_err());
    assert!(dec.decode(&[json!(["s", 3])]).is_err());
    assert_eq!(
        enc.encode(&ops(&json!([["s", ["a"], 4]]))).unwrap(),
        vec![json!(["s", ["a"], 4])]
    );
}

#[test]
fn immutable_replacement_payload_and_batched_replay_are_independent() {
    let first = ops(&json!([["r",{"nested":{"value":1}}]]));
    let second = ops(&json!([["s", ["nested", "value"], 2]]));
    let value = apply_immutable_batches(None, [first.as_slice(), second.as_slice()]).unwrap();
    assert_eq!(value, json!({"nested":{"value":2}}));
    assert_eq!(first[0], Op::Replace(json!({"nested":{"value":1}})));
}

#[test]
fn arrays_remain_dense_and_large_splices_do_not_overflow_the_stack() {
    let items = vec![Value::Null; 300_000];
    let result = apply(json!([]), &[Op::Splice(vec![], 0, 0, items)]).unwrap();
    assert_eq!(result.as_array().unwrap().len(), 300_000);
}

#[test]
fn decoded_vocabulary_rejects_wire_only_forms() {
    for value in [
        json!(["s", 1]),
        json!(["d"]),
        json!(["a", "x"]),
        json!(["t", 2]),
        json!(["p", 0, 0, []]),
        json!(["#", 0, ["v"]]),
        json!(["s", 0, 1]),
    ] {
        assert!(Op::from_value(&value).is_err());
        assert!(assert_valid_wire_op(&value).is_ok());
    }
}

// ---- Ports of packages/chord/test/delta.test.ts (named after the upstream cases) ----

use pi_chord::JsonValue;
use pi_chord::json::parse;
use std::sync::Arc;

fn jv(s: &str) -> JsonValue {
    parse(s).unwrap()
}
fn path(segments: &[&str]) -> Path {
    segments.iter().map(|s| Seg::from(*s)).collect()
}
fn same_object(a: &Value, b: &Value) -> bool {
    std::ptr::eq(a.as_object().unwrap(), b.as_object().unwrap())
}
fn is_unsafe(error: Error) -> bool {
    matches!(error, Error::UnsafePath(_))
}
fn is_path(error: Error) -> bool {
    matches!(error, Error::Path(_))
}

#[test]
fn immutable_tracker_lifecycle__aborts_and_revokes_without_changing_the_committed_value() {
    let mut tracker = track(jv(r#"{"child":{"value":1}}"#));
    let mut change = tracker.begin_change();
    *change.state().unwrap() = jv(r#"{"child":{"value":2}}"#);
    change.abort();
    assert_eq!(tracker.value()["child"]["value"], json!(1));
    assert!(change.state().is_err());
    change.abort();
    assert!(
        change
            .prepare()
            .err()
            .unwrap()
            .to_string()
            .contains("settled")
    );
    let mut next = tracker.begin_change();
    next.abort();
    let _ = &mut tracker;
}

#[test]
fn immutable_tracker_lifecycle__grows_arrays_with_explicit_nulls_and_revokes_drafts_after_preparation()
 {
    let mut tracker = track(jv(r#"{"values":[1,2]}"#));
    let mut change = tracker.begin_change();
    *change.state().unwrap() = jv(r#"{"values":[1,2,null,null]}"#);
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.value()["values"], json!([1, 2, null, null]));
    assert_eq!(
        to_value(prepared.ops()).unwrap(),
        json!([["p", ["values"], 2, 0, [null, null]]])
    );
    assert!(change.state().is_err());
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn immutable_tracker_lifecycle__shares_immutable_operation_placements_with_the_prepared_candidate()
{
    let tracker = track(jv(r#"{"rows":[]}"#));
    let mut change = tracker.begin_change();
    *change.state().unwrap() = jv(r#"{"rows":[{"id":1}]}"#);
    let prepared = change.prepare().unwrap();
    let Op::Splice(_, _, _, items) = &prepared.ops()[0] else {
        panic!("expected splice")
    };
    assert!(same_object(&items[0], &prepared.value()["rows"][0]));
}

#[test]
fn immutable_tracker_lifecycle__rejects_foreign_stale_and_repeated_preparations_while_allowing_competing_changes()
 {
    let mut first = track(jv(r#"{"value":0}"#));
    let mut second = track(jv(r#"{"value":0}"#));
    let mut change = first.begin_change();
    let mut competing = first.begin_change();
    *change.state().unwrap() = jv(r#"{"value":1}"#);
    *competing.state().unwrap() = jv(r#"{"value":2}"#);
    let prepared = change.prepare().unwrap();
    let competing_prepared = competing.prepare().unwrap();
    let message = |e: Error| e.to_string();
    assert!(message(second.adopt(&prepared).unwrap_err()).contains("different tracker"));
    first.adopt(&prepared).unwrap();
    assert!(message(first.adopt(&prepared).unwrap_err()).contains("already been used"));
    assert!(message(first.adopt(&competing_prepared).unwrap_err()).contains("stale"));

    let stale = first.prepare_replace(Arc::new(jv(r#"{"value":2}"#)));
    let winner = first.prepare_replace(Arc::new(jv(r#"{"value":3}"#)));
    first.adopt(&winner).unwrap();
    assert!(message(first.adopt(&stale).unwrap_err()).contains("stale"));
    assert!(message(first.adopt(&stale).unwrap_err()).contains("stale"));
}

#[test]
fn immutable_tracker_lifecycle__invalidates_a_prepared_result_when_its_change_is_aborted() {
    let mut tracker = track(jv(r#"{"value":0}"#));
    let mut change = tracker.begin_change();
    *change.state().unwrap() = jv(r#"{"value":1}"#);
    let prepared = change.prepare().unwrap();
    change.abort();
    assert!(
        tracker
            .adopt(&prepared)
            .unwrap_err()
            .to_string()
            .contains("aborted")
    );
    change.abort();
}

#[test]
fn immutable_tracker_lifecycle__makes_competing_same_base_preparations_stale_after_adopting_a_no_op()
 {
    let mut tracker = track(jv(r#"{"value":{"count":1}}"#));
    let first = tracker.prepare_replace(Arc::new(jv(r#"{"value":{"count":1}}"#)));
    let competing = tracker.prepare_replace(Arc::new(jv(r#"{"value":{"count":1}}"#)));
    assert!(Arc::ptr_eq(first.value(), first.base()));
    assert!(Arc::ptr_eq(competing.value(), competing.base()));
    tracker.adopt(&first).unwrap();
    assert!(
        tracker
            .adopt(&first)
            .unwrap_err()
            .to_string()
            .contains("already been used")
    );
    assert!(
        tracker
            .adopt(&competing)
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
}

#[test]
fn immutable_tracker_lifecycle__emits_an_owned_root_replacement_without_traversing_large_replacement_input()
 {
    let rows: Vec<Value> = (0..10_000)
        .map(|v| json!({"value": v, "stable": {"value": v}}))
        .collect();
    let tracker = track(json!({"rows": rows.clone()}));
    let mut changed = rows;
    changed[5_000] = json!({"value": -1, "stable": {"value": 5000}});
    let replacement = Arc::new(json!({"rows": changed}));
    let prepared = tracker.prepare_replace(replacement.clone());
    assert!(Arc::ptr_eq(prepared.value(), &replacement));
    assert_eq!(prepared.ops(), &[Op::Replace((*replacement).clone())]);
    assert_eq!(
        apply_immutable(Some(prepared.base()), prepared.ops()).unwrap(),
        **prepared.value()
    );
}

#[test]
fn canonical_strings__emits_append_and_rolling_window_operations() {
    let mut tracker = track(jv(r#"{"text":"abcdefgh"}"#));
    let mut change = tracker.begin_change();
    *change.state().unwrap() = jv(r#"{"text":"abcdefghij"}"#);
    let prepared = change.prepare().unwrap();
    assert_eq!(
        to_value(prepared.ops()).unwrap(),
        json!([["a", ["text"], "ij"]])
    );
    tracker.adopt(&prepared).unwrap();

    let mut change = tracker.begin_change();
    *change.state().unwrap() = jv(r#"{"text":"defghijxyz"}"#);
    let prepared = change.prepare().unwrap();
    assert_eq!(
        to_value(prepared.ops()).unwrap(),
        json!([["t", ["text"], 3], ["a", ["text"], "xyz"]])
    );
}

#[test]
fn canonical_strings__finds_bounded_overlaps() {
    assert_eq!(overlap("abcdefgh", "defghxyz", 65_536, 64, 8), 5);
    assert_eq!(overlap("abcdef", "defghi", 0, 64, 8), 0);
}

#[test]
fn operation_application_and_validation__applies_mutable_and_immutable_operations() {
    let operations = ops(&json!([
        ["a", ["text"], "b"],
        ["p", ["values"], 1, 1, [3, 4]],
        ["s", ["nested", "value"], 2]
    ]));
    let base = json!({"text":"a","values":[1,2],"nested":{"value":1},"stable":{"value":9}});
    let immutable = apply_immutable(Some(&base), &operations).unwrap();
    assert_eq!(
        immutable,
        json!({"text":"ab","values":[1,3,4],"nested":{"value":2},"stable":{"value":9}})
    );
    assert_eq!(
        base,
        json!({"text":"a","values":[1,2],"nested":{"value":1},"stable":{"value":9}})
    );
    assert!(same_object(&immutable["stable"], &base["stable"]));
    assert_eq!(apply(base.clone(), &operations).unwrap(), immutable);
}

#[test]
fn operation_application_and_validation__supports_root_replacement_root_splice_and_permutations() {
    let base = ops(&json!([["r", [1, 2, 3]]]));
    assert!(is_base(&base));
    let mut value = apply(Value::Null, &base).unwrap();
    value = apply(value, &ops(&json!([["p", [], 1, 1, [4]]]))).unwrap();
    value = apply(value, &ops(&json!([["m", [], [2, 0, 1]]]))).unwrap();
    assert_eq!(value, json!([3, 1, 4]));
}

#[test]
fn operation_application_and_validation__rejects_unsafe_and_malformed_paths() {
    let unsafe_op = Op::Set(path(&["constructor", "prototype", "x"]), json!(true));
    assert!(is_unsafe(apply(json!({}), &[unsafe_op]).unwrap_err()));
    assert!(is_unsafe(
        apply(
            json!({"values":[1]}),
            &ops(&json!([["s", ["values", 3], 2]]))
        )
        .unwrap_err()
    ));
    assert!(apply(json!({"value":1}), &ops(&json!([["a", ["value"], "x"]]))).is_err());
    assert!(Op::from_value(&json!(["s", "value", 1])).is_err());
    assert!(
        Op::from_value(&json!(["m", [], [0, 0]]))
            .unwrap_err()
            .to_string()
            .contains("bijection")
    );
}

#[test]
fn operation_application_and_validation__validates_decoded_and_wire_vocabularies_separately() {
    assert!(Op::from_value(&json!(["s", ["value"], 1])).is_ok());
    assert!(Op::from_value(&json!(["s", 1])).is_err());
    assert!(assert_valid_wire_op(&json!(["s", 1])).is_ok());
    assert!(assert_valid_wire_op(&json!(["#", 0, ["value"]])).is_ok());
}

#[test]
fn codec__interns_paths_omits_adjacent_paths_and_round_trips() {
    let mut enc = Encoder::new();
    let mut dec = Decoder::new();
    let first = ops(&json!([
        ["t", ["nested", "text"], 1],
        ["a", ["nested", "text"], "x"]
    ]));
    assert_eq!(dec.decode(&enc.encode(&first).unwrap()).unwrap(), first);
    let second = ops(&json!([["a", ["nested", "text"], "y"]]));
    let wire = enc.encode(&second).unwrap();
    assert_eq!(
        wire,
        vec![json!(["#", 0, ["nested", "text"]]), json!(["a", 0, "y"])]
    );
    assert_eq!(dec.decode(&wire).unwrap(), second);
}

#[test]
fn codec__resets_path_dictionaries_on_a_base() {
    let mut enc = Encoder::new();
    enc.encode(&ops(&json!([["s", ["value"], 1]]))).unwrap();
    enc.encode(&ops(&json!([["s", ["value"], 2]]))).unwrap();
    assert_eq!(
        enc.encode(&ops(&json!([["r", {"value": 3}]]))).unwrap(),
        vec![json!(["r", {"value": 3}])]
    );
    assert_eq!(
        enc.encode(&ops(&json!([["s", ["value"], 4]]))).unwrap(),
        vec![json!(["s", ["value"], 4])]
    );
}

#[test]
fn codec__rejects_unresolved_short_forms_and_unsafe_interned_paths() {
    assert!(Decoder::new().decode(&[json!(["a", "x"])]).is_err());
    let wire = [json!(["#", 0, ["__proto__"]]), json!(["s", 0, true])];
    assert!(is_unsafe(Decoder::new().decode(&wire).unwrap_err()));
}

#[test]
fn codec__omits_an_adjacent_repeated_path() {
    let mut enc = Encoder::new();
    assert_eq!(
        enc.encode(&ops(&json!([["s", ["value"], 1], ["s", ["value"], 2]])))
            .unwrap(),
        vec![json!(["s", ["value"], 1]), json!(["s", 2])]
    );
}

#[test]
fn codec__interns_on_second_use_rather_than_first() {
    let mut enc = Encoder::new();
    assert_eq!(
        enc.encode(&ops(&json!([["a", ["a", "deep"], "1"]])))
            .unwrap(),
        vec![json!(["a", ["a", "deep"], "1"])]
    );
    assert_eq!(
        enc.encode(&ops(&json!([["a", ["a", "deep"], "2"]])))
            .unwrap(),
        vec![json!(["#", 0, ["a", "deep"]]), json!(["a", 0, "2"])]
    );
}

#[test]
fn codec__does_not_collide_paths_containing_null_characters() {
    let operations = ops(&json!([["s", ["a\u{0}b"], 1], ["s", ["a", "b"], 2]]));
    let wire = Encoder::new().encode(&operations).unwrap();
    assert_eq!(Decoder::new().decode(&wire).unwrap(), operations);
}

#[test]
fn codec__clears_decoder_ids_on_a_base_batch() {
    let mut dec = Decoder::new();
    dec.decode(&[json!(["#", 0, ["a"]]), json!(["a", 0, "1"])])
        .unwrap();
    dec.decode(&[json!(["r", {"a": ""}])]).unwrap();
    assert!(dec.decode(&[json!(["a", 0, "2"])]).is_err());
}

#[test]
fn codec__makes_batches_after_a_base_self_contained() {
    let mut enc = Encoder::new();
    let one = ops(&json!([["a", ["a", "deep"], "1"]]));
    let two = ops(&json!([["a", ["a", "deep"], "2"]]));
    enc.encode(&one).unwrap();
    enc.encode(&two).unwrap();
    let base = enc
        .encode(&ops(&json!([["r", {"a": {"deep": "x"}}]])))
        .unwrap();
    let three = ops(&json!([["a", ["a", "deep"], "3"]]));
    let after = enc.encode(&three).unwrap();
    assert_eq!(after, vec![json!(["a", ["a", "deep"], "3"])]);
    let mut dec = Decoder::new();
    assert_eq!(
        dec.decode(&base).unwrap(),
        ops(&json!([["r", {"a": {"deep": "x"}}]]))
    );
    assert_eq!(dec.decode(&after).unwrap(), three);
}

#[test]
fn codec__round_trips_deterministic_mixed_operation_streams() {
    let batches: Vec<Vec<Op>> = (0..100)
        .map(|index| {
            ops(&json!([
                ["s", ["rows", index, "value"], index],
                ["a", ["output"], index.to_string()],
                ["p", ["tail"], index, 0, [index]]
            ]))
        })
        .collect();
    let mut enc = Encoder::new();
    let mut dec = Decoder::new();
    let decoded: Vec<_> = batches
        .iter()
        .map(|batch| dec.decode(&enc.encode(batch).unwrap()).unwrap())
        .collect();
    assert_eq!(decoded, batches);
}

#[test]
fn immutable_application_ownership__does_not_mutate_a_replacement_payload_targeted_by_a_later_operation()
 {
    let replacement = json!({"nested":{"value":1}});
    let next = apply_immutable(
        None,
        &[
            Op::Replace(replacement.clone()),
            Op::Set(path(&["nested", "value"]), json!(2)),
        ],
    )
    .unwrap();
    assert_eq!(replacement["nested"]["value"], json!(1));
    assert_eq!(next["nested"]["value"], json!(2));
}

#[test]
fn path_and_prototype_safety__rejects_constructor_walks_and_forbidden_interned_paths() {
    assert!(
        serde_json::from_str::<Vec<Op>>(r#"[["s",["constructor","prototype","gadget"],true]]"#)
            .is_err()
    );
    let wire = [json!(["#", 0, ["__proto__", "w"]]), json!(["s", 0, true])];
    assert!(Decoder::new().decode(&wire).is_err());
}

#[test]
fn path_and_prototype_safety__allows_reserved_names_inside_values_without_prototype_pollution() {
    let value = jv(r#"{"__proto__":{"z":1}}"#);
    let out = apply(json!({}), &[Op::Set(path(&["value"]), value.clone())]).unwrap();
    assert_eq!(out["value"], value);
    assert!(
        out["value"]
            .as_object()
            .unwrap()
            .contains_key(&pi_chord::json::JsonString::from("__proto__"))
    );
    assert!(
        json!({})
            .as_object()
            .unwrap()
            .get(&pi_chord::json::JsonString::from("z"))
            .is_none()
    );
}

#[test]
fn array_index_safety__writes_an_existing_index_and_appends_exactly_one_past_the_end() {
    let set = |index: usize| ops(&json!([["s", ["values", index], 9]]));
    assert_eq!(
        apply(json!({"values":[1,2,3]}), &set(1)).unwrap(),
        json!({"values":[1,9,3]})
    );
    assert_eq!(
        apply(json!({"values":[1,2,3]}), &set(3)).unwrap(),
        json!({"values":[1,2,3,9]})
    );
}

#[test]
fn array_index_safety__rejects_gaps_huge_indices_and_string_spelled_indices() {
    let run = |initial: Value, op: Value| apply(initial, &ops(&json!([op])));
    assert!(run(json!({"values":[1,2,3]}), json!(["s", ["values", 5], 9])).is_err());
    assert!(
        run(
            json!({"values":[]}),
            json!(["s", ["values", 4_294_967_290u64], 1])
        )
        .is_err()
    );
    assert!(run(json!({"values":[1]}), json!(["s", ["values", "0"], 9])).is_err());
    assert!(run(json!({"values":["a"]}), json!(["a", ["values", "0"], "b"])).is_err());
}

#[test]
fn array_index_safety__allows_explicit_growth_values_and_rejects_deletion_past_the_end() {
    assert_eq!(
        apply(
            json!({"values":[1]}),
            &ops(&json!([["p", ["values"], 1, 0, [null, null, 9]]]))
        )
        .unwrap(),
        json!({"values":[1, null, null, 9]})
    );
    assert!(apply(json!({"values":[1]}), &ops(&json!([["d", ["values", 1]]]))).is_err());
}

#[test]
fn array_index_safety__applies_large_splice_payloads_without_spreading_them_at_once() {
    let items = vec![Value::Null; 300_000];
    let result = apply(
        json!({"values":[]}),
        &[Op::Splice(path(&["values"]), 0, 0, items.clone())],
    )
    .unwrap();
    assert_eq!(result["values"].as_array().unwrap().len(), items.len());
}

#[test]
fn operation_structure_safety__rejects_unknown_verbs_malformed_tuples_paths_and_splice_payloads() {
    for raw in [
        json!(["ZZZ", ["value"], 9]),
        json!(["p", ["values"], 0, 0, "not-an-array"]),
        json!(["s", "value", 9]),
        json!({"op": "s"}),
        Value::Null,
    ] {
        assert!(Op::from_value(&raw).is_err(), "{raw}");
    }
}

#[test]
fn operation_structure_safety__rejects_invalid_append_and_truncation_operations() {
    let run = |initial: Value, op: Value| apply(initial, &ops(&json!([op])));
    assert!(run(json!({"value":1}), json!(["a", ["missing"], "x"])).is_err());
    assert!(run(json!({"value":1}), json!(["a", ["value"], "x"])).is_err());
    assert!(Op::from_value(&json!(["t", ["value"], -1])).is_err());
    assert!(
        Decoder::new()
            .decode(&[json!(["t", ["value"], -1])])
            .is_err()
    );
}

#[test]
fn operation_structure_safety__clamps_splice_removal_past_the_end() {
    assert_eq!(
        apply(
            json!({"values":[1,2]}),
            &ops(&json!([["p", ["values"], 0, 1e9, []]]))
        )
        .unwrap(),
        json!({"values":[]})
    );
}

#[test]
fn operation_assertions__accepts_decoded_operations_and_rejects_wire_only_forms() {
    for operation in [
        json!(["r", {"value": 1}]),
        json!(["s", ["value"], 1]),
        json!(["d", ["value"]]),
        json!(["a", ["value"], "x"]),
        json!(["t", ["value"], 2]),
        json!(["p", ["value"], 0, 0, []]),
        json!(["m", ["value"], [0]]),
    ] {
        assert!(Op::from_value(&operation).is_ok(), "{operation}");
    }
    for wire_only in [
        json!(["s", 1]),
        json!(["d"]),
        json!(["a", "x"]),
        json!(["t", 2]),
        json!(["p", 0, 0, []]),
        json!(["#", 0, ["value"]]),
        json!(["s", 0, 1]),
    ] {
        assert!(Op::from_value(&wire_only).is_err(), "{wire_only}");
        assert!(assert_valid_wire_op(&wire_only).is_ok(), "{wire_only}");
    }
}

// ---- Ports of packages/chord/test/delta-apply-immutable.test.ts ----

fn set(segments: &[&str], value: Value) -> Op {
    Op::Set(path(segments), value)
}
fn at(segments: &[Seg], value: Value) -> Op {
    Op::Set(segments.to_vec(), value)
}

#[test]
fn checked_immutable_operation_application__copies_each_touched_container_once_while_preserving_input_and_payload_ownership()
 {
    let shared = json!({"nested":{"value":1}});
    let untouched = json!({"value":9});
    let row_payload = json!({"id":4,"label":"placed"});
    let base = json!({
        "text": "abcdef",
        "stable": {"value": 7},
        "branch": {"value": 1},
        "copy": null, "placed": null, "untouched": null, "left": null, "right": null,
        "meta": {"count": 0, "obsolete": true},
        "rows": [
            {"id": 1, "label": "one"},
            {"id": 2, "label": "two"},
            {"id": 3, "label": "three"}
        ]
    });
    let operations = vec![
        Op::Truncate(path(&["text"]), 2),
        Op::Append(path(&["text"]), "!".into()),
        set(&["meta", "count"], json!(1)),
        set(&["meta", "count"], json!(2)),
        Op::Delete(path(&["meta", "obsolete"])),
        set(&["copy"], base["branch"].clone()),
        set(&["copy", "value"], json!(2)),
        set(&["placed"], shared.clone()),
        set(&["placed", "nested", "value"], json!(2)),
        set(&["untouched"], untouched.clone()),
        set(&["left"], shared.clone()),
        set(&["right"], shared.clone()),
        set(&["left", "nested", "value"], json!(3)),
        Op::Splice(path(&["rows"]), 1, 1, vec![row_payload.clone()]),
        at(&["rows".into(), 1.into(), "label".into()], json!("edited")),
        Op::Move(path(&["rows"]), vec![1, 0, 2]),
        at(&["rows".into(), 0.into(), "label".into()], json!("moved")),
    ];
    let result = apply_immutable(Some(&base), &operations).unwrap();
    let mutable_result = apply(base.clone(), &operations).unwrap();
    assert_eq!(result, mutable_result);
    assert_eq!(result["text"], json!("cdef!"));
    assert!(same_object(&result["stable"], &base["stable"]));
    assert!(same_object(&result["untouched"], &untouched));
    assert!(!same_object(&result["copy"], &base["branch"]));
    assert_eq!(result["copy"], json!({"value":2}));
    assert!(!same_object(&result["placed"], &shared));
    assert_eq!(result["placed"], json!({"nested":{"value":2}}));
    assert!(!same_object(&result["left"], &shared));
    assert!(same_object(&result["right"], &shared));
    assert!(!same_object(&result["rows"][0], &row_payload));
    assert_eq!(result["rows"][0], json!({"id":4,"label":"moved"}));
    assert_eq!(base["branch"]["value"], json!(1));
    assert_eq!(shared["nested"]["value"], json!(1));
    assert_eq!(row_payload["label"], json!("placed"));
}

#[test]
fn checked_immutable_operation_application__protects_root_replacement_payloads_before_later_object_and_array_edits()
 {
    let replacement = json!({"nested":{"value":1},"values":[1,2,3]});
    let first = [Op::Replace(replacement.clone())];
    let second = [set(&["nested", "value"], json!(2))];
    let third = ops(&json!([
        ["p", ["values"], 1, 1, [4, 5]],
        ["m", ["values"], [3, 0, 1, 2]]
    ]));
    let result = apply_immutable_batches(
        None,
        [first.as_slice(), second.as_slice(), third.as_slice()],
    )
    .unwrap();
    assert_eq!(result, json!({"nested":{"value":2},"values":[3,1,4,5]}));
    assert_eq!(replacement, json!({"nested":{"value":1},"values":[1,2,3]}));

    let array = json!([1, 2, 3]);
    let array_result = apply_immutable(
        Some(&array),
        &ops(&json!([
            ["p", [], 1, 1, [4, 5]],
            ["m", [], [3, 0, 1, 2]],
            ["d", [1]]
        ])),
    )
    .unwrap();
    assert_eq!(array_result, json!([3, 4, 5]));
    assert_eq!(array, json!([1, 2, 3]));
}

#[test]
fn checked_immutable_operation_application__shares_one_private_copy_on_write_scope_across_batch_partitions()
 {
    let base = json!({
        "text": "abcdef", "meta": {"count": 0},
        "values": [{"id":1,"value":1},{"id":2,"value":2},{"id":3,"value":3}]
    });
    let batches = [
        ops(&json!([["s", ["meta", "count"], 1], ["p", ["values"], 1, 1, [{"id":4,"value":4}]]])),
        vec![],
        ops(&json!([
            ["m", ["values"], [2, 0, 1]],
            ["s", ["values", 2, "value"], 40]
        ])),
        ops(&json!([["t", ["text"], 2], ["a", ["text"], "!"]])),
    ];
    let intermediate = apply_immutable(Some(&base), &batches[0]).unwrap();
    let snapshot = intermediate.clone();
    let mut sequential = intermediate.clone();
    for batch in &batches[1..] {
        sequential = apply_immutable(Some(&sequential), batch).unwrap();
    }
    let streamed = apply_immutable_batches(Some(&base), batches.iter().map(Vec::as_slice)).unwrap();
    let flattened: Vec<Op> = batches.iter().flatten().cloned().collect();
    let flat = apply_immutable(Some(&base), &flattened).unwrap();
    assert_eq!(streamed, sequential);
    assert_eq!(streamed, flat);
    assert_eq!(intermediate, snapshot);
    assert_eq!(base["meta"]["count"], json!(0));
    assert_eq!(base["values"][1]["id"], json!(2));
}

#[test]
fn checked_immutable_operation_application__handles_tracker_produced_batches_across_arbitrary_revision_boundaries()
 {
    let mut doc = serde_json::json!({
        "text": "start",
        "values": (0..8).map(|id| serde_json::json!({"id": id, "score": 0})).collect::<Vec<_>>(),
        "revision": 0
    });
    let initial = Value::from(doc.clone());
    let mut tracker = track(initial.clone());
    let mut batches: Vec<Vec<Op>> = Vec::new();
    for revision in 1..=40usize {
        let mut change = tracker.begin_change();
        doc["revision"] = revision.into();
        let text = doc["text"].as_str().unwrap()[1..].to_string() + &revision.to_string();
        doc["text"] = text.into();
        let values = doc["values"].as_array_mut().unwrap();
        match revision % 5 {
            0 => values.reverse(),
            1 => values.push(serde_json::json!({"id": 100 + revision, "score": revision})),
            2 => {
                values.remove(0);
            }
            3 => {
                let index = revision % values.len();
                values[index]["score"] = revision.into();
            }
            _ => {
                values[1] = serde_json::json!({"id": 200 + revision, "score": revision});
            }
        }
        *change.state().unwrap() = Value::from(doc.clone());
        let prepared = change.prepare().unwrap();
        batches.push(prepared.ops().to_vec());
        tracker.adopt(&prepared).unwrap();
    }
    assert_eq!(
        apply_immutable_batches(Some(&initial), batches.iter().map(Vec::as_slice)).unwrap(),
        **tracker.value()
    );
}

struct Probe<'a> {
    batches: &'a [Vec<Op>],
    next: usize,
    advanced_past_invalid: &'a std::cell::Cell<bool>,
    closed: &'a std::cell::Cell<bool>,
}
impl<'a> Iterator for Probe<'a> {
    type Item = &'a [Op];
    fn next(&mut self) -> Option<&'a [Op]> {
        if self.next == self.batches.len() {
            self.advanced_past_invalid.set(true);
            return None;
        }
        self.next += 1;
        Some(&self.batches[self.next - 1])
    }
}
impl Drop for Probe<'_> {
    fn drop(&mut self) {
        self.closed.set(true);
    }
}

#[test]
fn checked_immutable_operation_application__does_not_expose_partial_application_when_validation_or_iteration_fails()
 {
    let base = json!({"nested":{"value":1}});
    let invalid = [
        ops(&json!([["s", ["nested", "value"], 2]])),
        vec![set(&["constructor", "prototype", "polluted"], json!(true))],
    ];
    assert!(apply_immutable_batches(Some(&base), invalid.iter().map(Vec::as_slice)).is_err());
    assert_eq!(base["nested"]["value"], json!(1));

    let batches = [
        ops(&json!([["s", ["nested", "value"], 4]])),
        vec![set(&["__proto__", "polluted"], json!(true))],
    ];
    let advanced = std::cell::Cell::new(false);
    let closed = std::cell::Cell::new(false);
    let probe = Probe {
        batches: &batches,
        next: 0,
        advanced_past_invalid: &advanced,
        closed: &closed,
    };
    assert!(is_unsafe(
        apply_immutable_batches(Some(&base), probe).unwrap_err()
    ));
    assert!(!advanced.get());
    assert!(closed.get());
    assert_eq!(base["nested"]["value"], json!(1));

    assert!(is_path(
        apply_immutable(
            Some(&json!({"values":[]})),
            &ops(&json!([["s", ["values", "missing", "value"], 1]]))
        )
        .unwrap_err()
    ));
    assert!(is_unsafe(
        apply_immutable(
            Some(&json!({"values":[{}]})),
            &ops(&json!([["s", ["values", "0", "value"], 1]]))
        )
        .unwrap_err()
    ));
}

#[test]
fn checked_immutable_operation_application__allows_one_immutable_batch_to_fan_out_without_mutating_shared_payloads()
 {
    let payload = json!({"nested":{"value":1}});
    let operations = vec![
        set(&["placed"], payload.clone()),
        set(&["placed", "nested", "value"], json!(2)),
    ];
    let base = json!({"placed": null});
    let first = apply_immutable(Some(&base), &operations).unwrap();
    let second = apply_immutable(Some(&base), &operations).unwrap();
    assert_eq!(first, second);
    assert!(!same_object(&first, &second));
    assert!(!same_object(&first["placed"], &second["placed"]));
    assert_eq!(payload["nested"]["value"], json!(1));
}

#[test]
fn immutable_application_complexity__appends_to_an_array_not_yet_copied_in_one_copy_sharing_everything_else()
 {
    let entries: Vec<Value> = (0..1_000).map(|index| json!({"index": index})).collect();
    let base = json!({"log": {"entries": entries, "meta": {"count": 1000}}, "other": {"value": 1}});
    let result = apply_immutable(
        Some(&base),
        &ops(&json!([
            ["p", ["log", "entries"], 1000, 0, [{"index": 1000}]],
            ["p", ["log", "entries"], 1001, 0, [{"index": 1001}]]
        ])),
    )
    .unwrap();
    let out = result["log"]["entries"].as_array().unwrap();
    assert_eq!(out.len(), 1_002);
    assert_eq!(out[1000..], [json!({"index":1000}), json!({"index":1001})]);
    assert_eq!(base["log"]["entries"].as_array().unwrap().len(), 1_000);
    assert!(same_object(&out[0], &base["log"]["entries"][0]));
    assert!(same_object(&result["log"]["meta"], &base["log"]["meta"]));
    assert!(same_object(&result["other"], &base["other"]));
    assert!(!same_object(&result["log"], &base["log"]));

    let root_array = json!([1, 2]);
    let run = |value: &Value, op: Value| apply_immutable(Some(value), &ops(&json!([op])));
    assert_eq!(
        run(&root_array, json!(["p", [], 2, 0, [3]])).unwrap(),
        json!([1, 2, 3])
    );
    assert_eq!(root_array, json!([1, 2]));
    // An appended item that is itself an array stays one element.
    assert_eq!(
        run(&root_array, json!(["p", [], 2, 0, [[3, 4]]])).unwrap(),
        json!([1, 2, [3, 4]])
    );
    // Invalid paths fail with the general path's errors.
    let document = json!({"values": [], "list": [[1]], "text": "a"});
    for (op, expect_unsafe) in [
        (json!(["p", ["values", "missing"], 0, 0, []]), false),
        (json!(["p", ["missing"], 0, 0, []]), false),
        (json!(["p", ["values", 0], 0, 0, []]), false),
        (json!(["p", ["__proto__"], 0, 0, []]), true),
        (json!(["p", ["list", "x"], 0, 0, []]), false),
        (json!(["p", ["text"], 0, 0, []]), false),
        (json!(["p", ["list", -1], 0, 0, []]), true),
        (json!(["p", ["list", 1.5], 0, 0, []]), true),
        (json!(["p", ["list", 0, "constructor"], 0, 0, []]), true),
    ] {
        let error = match serde_json::from_str::<Vec<Op>>(&json!([op.clone()]).to_string()) {
            Ok(parsed) => apply_immutable(Some(&document), &parsed).unwrap_err(),
            // Negative and fractional indices are rejected when the tuple is decoded.
            Err(error) => Error::UnsafePath(error.to_string()),
        };
        assert_eq!(is_unsafe(error.clone()), expect_unsafe, "{op}: {error}");
        assert!(expect_unsafe || is_path(error), "{op}");
    }
    // An insert that is not at the end takes the general path.
    assert_eq!(
        run(&root_array, json!(["p", [], 1, 0, [9]])).unwrap(),
        json!([1, 9, 2])
    );
}

#[test]
fn immutable_application_complexity__copies_a_wide_object_once_rather_than_once_per_repeated_write()
{
    // The upstream case counts Object.keys calls, which has no Rust equivalent;
    // this pins the observable contract: results, untouched input, shared siblings.
    let width = 20_000;
    let base = Value::Object(
        (0..width)
            .map(|index| (format!("field{index}").into(), json!(index)))
            .collect::<pi_chord::json::Map>()
            .into(),
    );
    let operations: Vec<Op> = (0..1_000i64)
        .map(|index| set(&[&format!("field{index}")], json!(-index)))
        .collect();
    let result = apply_immutable(Some(&base), &operations).unwrap();
    assert_eq!(result["field999"], json!(-999));
    assert_eq!(base["field999"], json!(999));
    assert_eq!(result["field19999"], base["field19999"]);
}
