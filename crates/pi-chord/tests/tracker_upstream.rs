//! Native counterparts of `packages/chord/test/delta-tracker/{tracker,retention}.test.ts`.
//! Draft-proxy, JS identity and heap-measurement cases are recorded as embedded-only in
//! `porting/durable/mappings/chord-c.json`; the cases here hold the contracts the Rust API owns.
use pi_chord::delta::*;
use pi_chord::{
    JsonValue,
    json::{parse, to_value},
};
use std::sync::Arc;

fn value(s: &str) -> JsonValue {
    parse(s).unwrap()
}
fn wire(ops: &[Op]) -> JsonValue {
    to_value(ops).unwrap()
}
fn rows(count: usize, field: impl Fn(usize) -> i64) -> String {
    let items: Vec<String> = (0..count)
        .map(|i| format!(r#"{{"value":{}}}"#, field(i)))
        .collect();
    format!("[{}]", items.join(","))
}

/// Mirrors the upstream `settle` helper: assign the next draft, check replay and base
/// immutability, adopt, and return the prepared candidate.
fn settle(tracker: &mut Tracker, next: &str) -> Prepared {
    let base = tracker.value().clone();
    let snapshot = base.as_ref().clone();
    let mut change = tracker.begin_change();
    *change.state().unwrap() = value(next);
    let prepared = change.prepare().unwrap();
    assert_eq!(**prepared.value(), value(next));
    assert_eq!(
        apply_immutable(Some(&base), prepared.ops()).unwrap(),
        **prepared.value()
    );
    assert_eq!(*base, snapshot);
    tracker.adopt(&prepared).unwrap();
    assert_eq!(*base, snapshot);
    assert!(Arc::ptr_eq(tracker.value(), prepared.value()));
    prepared
}
fn prepare_next(tracker: &Tracker, next: &str) -> Prepared {
    let mut change = tracker.begin_change();
    *change.state().unwrap() = value(next);
    change.prepare().unwrap()
}
fn num(n: i64) -> JsonValue {
    JsonValue::from(serde_json::json!(n))
}
/// Copy-on-write edit of one path inside an owned draft.
fn set_at(root: &mut JsonValue, path: &[Seg], new: JsonValue) {
    let Some((head, rest)) = path.split_first() else {
        *root = new;
        return;
    };
    let child = match head {
        Seg::Key(key) => root.as_object_mut().unwrap().get_mut(key).unwrap(),
        Seg::Index(index) => &mut root.as_array_mut().unwrap()[*index],
    };
    set_at(child, rest, new);
}
fn field<'a>(value: &'a mut JsonValue, key: &str) -> &'a mut JsonValue {
    value
        .as_object_mut()
        .unwrap()
        .get_mut(&pi_chord::JsonString::from(key))
        .unwrap()
}
fn err(result: Result<(), Error>) -> String {
    result.unwrap_err().to_string()
}

// ---- delta tracker transactional overlay lifecycle ----

#[test]
fn aborts_idempotently_and_revokes_held_descendants() {
    let tracker = track(value(r#"{"child":{"value":1}}"#));
    let mut change = tracker.begin_change();
    *change.state().unwrap() = value(r#"{"child":{"value":2}}"#);
    change.abort();
    change.abort();
    assert_eq!(tracker.value()["child"]["value"], JsonValue::from(1));
    assert!(change.state().is_err());
    assert!(change.current().is_err());
    assert!(change.prepare().is_err());
}

#[test]
fn allows_competing_contexts_and_invalidates_loser_views() {
    let mut tracker = track(value(r#"{"value":0}"#));
    let mut first = tracker.begin_change();
    let mut second = tracker.begin_change();
    *first.state().unwrap() = value(r#"{"value":1}"#);
    *second.state().unwrap() = value(r#"{"value":2}"#);
    let first_prepared = first.prepare().unwrap();
    let second_prepared = second.prepare().unwrap();
    let held = second_prepared.value().clone();
    tracker.adopt(&first_prepared).unwrap();
    assert_eq!(tracker.value()["value"], JsonValue::from(1));
    assert_eq!(held["value"], JsonValue::from(2));
    assert!(err(tracker.adopt(&second_prepared)).contains("stale"));
    assert!(err(tracker.adopt(&first_prepared)).contains("already been used"));
}

#[test]
fn invalidates_competing_open_overlays_without_changing_their_base_revision() {
    let initial = value(r#"{"rows":[{"value":0},{"value":1}]}"#);
    let mut tracker = track(initial.clone());
    let mut stale = tracker.begin_change();
    *stale.state().unwrap() = value(r#"{"rows":[{"value":0},{"value":2}]}"#);
    let mut winner = tracker.begin_change();
    *winner.state().unwrap() = value(r#"{"rows":[{"value":3},{"value":1}]}"#);
    let prepared = winner.prepare().unwrap();
    tracker.adopt(&prepared).unwrap();
    assert_eq!(initial, value(r#"{"rows":[{"value":0},{"value":1}]}"#));
    assert_eq!(
        **tracker.value(),
        value(r#"{"rows":[{"value":3},{"value":1}]}"#)
    );
    assert!(stale.state().is_err());
    assert!(stale.prepare().is_err());
    stale.abort();
}

#[test]
fn adopts_object_edits_as_ordinary_fast_layout_immutable_revisions() {
    let initial = Arc::new(value(r#"{"first":1,"second":2}"#));
    let mut tracker = Tracker::new(initial.clone());
    settle(&mut tracker, r#"{"first":3,"third":4}"#);
    assert!(!Arc::ptr_eq(tracker.value(), &initial));
    assert_eq!(*initial, value(r#"{"first":1,"second":2}"#));
    assert_eq!(**tracker.value(), value(r#"{"first":3,"third":4}"#));
    assert_eq!(tracker.value()["third"], JsonValue::from(4));
}

#[test]
fn supports_adopt_then_publish_ordering_and_retained_publication_reads() {
    let mut tracker = track(value(r#"{"value":0,"nested":{"count":0}}"#));
    let prepared = prepare_next(&tracker, r#"{"value":1,"nested":{"count":1}}"#);
    let operations = prepared.shared_ops();
    tracker.adopt(&prepared).unwrap();
    // adopt first, then inspect ops and publish prepared.value
    assert!(Arc::ptr_eq(&operations, &prepared.shared_ops()));
    assert_eq!(operations.len(), 2);
    let published = prepared.value().clone();
    assert!(Arc::ptr_eq(&published, tracker.value()));
    assert_eq!(*published, value(r#"{"value":1,"nested":{"count":1}}"#));

    let next = prepare_next(&tracker, r#"{"value":2,"nested":{"count":1}}"#);
    tracker.adopt(&next).unwrap();
    // retained immutable publications remain at their original revision
    assert!(!Arc::ptr_eq(&published, tracker.value()));
    assert_eq!(
        **prepared.value(),
        value(r#"{"value":1,"nested":{"count":1}}"#)
    );
    assert_eq!(
        **tracker.value(),
        value(r#"{"value":2,"nested":{"count":1}}"#)
    );
}

#[test]
fn keeps_replacement_base_value_structurally_compatible_and_readable_after_adoption() {
    let original = Arc::new(value(r#"{"value":0}"#));
    let replacement = Arc::new(value(r#"{"value":1}"#));
    let mut tracker = Tracker::new(original.clone());
    let prepared = tracker.prepare_replace(replacement.clone());
    assert!(Arc::ptr_eq(prepared.base(), &original));
    assert!(Arc::ptr_eq(prepared.value(), &replacement));
    tracker.adopt(&prepared).unwrap();
    assert!(Arc::ptr_eq(prepared.base(), &original));
    assert!(Arc::ptr_eq(prepared.value(), &replacement));
    assert!(Arc::ptr_eq(prepared.value(), tracker.value()));
}

#[test]
fn keeps_aborted_and_stale_materialized_candidates_readable() {
    let mut tracker = track(value(r#"{"value":0}"#));
    let aborted = prepare_next(&tracker, r#"{"value":1}"#);
    let aborted_view = aborted.value().clone();
    aborted.abort();
    assert_eq!(aborted_view["value"], JsonValue::from(1));
    assert_eq!(aborted.value()["value"], JsonValue::from(1));

    let loser = prepare_next(&tracker, r#"{"value":2}"#);
    let loser_view = loser.value().clone();
    let winner = tracker.prepare_replace(Arc::new(value(r#"{"value":3}"#)));
    tracker.adopt(&winner).unwrap();
    assert_eq!(loser_view["value"], JsonValue::from(2));
    assert_eq!(loser.value()["value"], JsonValue::from(2));
}

#[test]
fn reads_deleted_own_properties_as_absent() {
    // The prototype-inheritance half of the upstream case is JS-only; own-property absence is native.
    let mut tracker = track(value(r#"{"value":1}"#));
    let mut change = tracker.begin_change();
    *change.state().unwrap() = value(r#"{}"#);
    assert_eq!(change.current().unwrap().as_object().unwrap().len(), 0);
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.value().as_object().unwrap().len(), 0);
    assert_eq!(wire(prepared.ops()), value(r#"[["d",["value"]]]"#));
    tracker.adopt(&prepared).unwrap();
    assert_eq!(tracker.value().as_object().unwrap().len(), 0);
}

#[test]
fn deeply_equal_assignments_yield_no_operations_and_retain_committed_identity() {
    // Native half of "detaches old handles for deeply equal object and array assignments".
    let initial = Arc::new(value(
        r#"{"object":{"nested":{"value":1}},"array":[{"value":1}],"rows":[{"value":1}]}"#,
    ));
    let mut tracker = Tracker::new(initial.clone());
    let mut change = tracker.begin_change();
    *change.state().unwrap() =
        value(r#"{"object":{"nested":{"value":1}},"array":[{"value":1}],"rows":[{"value":1}]}"#);
    let prepared = change.prepare().unwrap();
    assert!(prepared.ops().is_empty());
    tracker.adopt(&prepared).unwrap();
    assert!(Arc::ptr_eq(tracker.value(), &initial));
    assert!(std::ptr::eq(
        tracker.value()["object"].as_object().unwrap(),
        initial["object"].as_object().unwrap()
    ));
    assert!(std::ptr::eq(
        tracker.value()["array"].as_array().unwrap(),
        initial["array"].as_array().unwrap()
    ));
}

#[test]
fn normalizes_a_deeply_equal_replacement_without_changing_committed_identity() {
    let initial = Arc::new(value(r#"{"nested":{"value":1},"rows":[1,2,3]}"#));
    let mut tracker = Tracker::new(initial.clone());
    let prepared =
        tracker.prepare_replace(Arc::new(value(r#"{"nested":{"value":1},"rows":[1,2,3]}"#)));
    assert!(prepared.ops().is_empty());
    tracker.adopt(&prepared).unwrap();
    assert!(Arc::ptr_eq(tracker.value(), &initial));
}

#[test]
fn takes_o1_immutable_ownership_for_replacement_and_its_operation_payload() {
    let mut tracker = track(value(r#"{"value":0,"rows":[]}"#));
    let replacement = Arc::new(value(r#"{"value":1,"rows":[{"value":2}]}"#));
    let prepared = tracker.prepare_replace(replacement.clone());
    assert!(Arc::ptr_eq(prepared.base(), tracker.value()));
    assert!(Arc::ptr_eq(prepared.value(), &replacement));
    let operations = prepared.shared_ops();
    assert_eq!(operations.len(), 1);
    let Op::Replace(payload) = &operations[0] else {
        panic!("expected a replace")
    };
    assert_eq!(payload, replacement.as_ref());
    assert!(std::ptr::eq(
        payload["rows"].as_array().unwrap(),
        replacement["rows"].as_array().unwrap()
    ));
    tracker.adopt(&prepared).unwrap();
    assert!(Arc::ptr_eq(tracker.value(), &replacement));
}

// ---- delta tracker policy view (native-relevant cases) ----

#[test]
fn keeps_document_keys_distinct_from_object_proxy_target_fields() {
    let mut tracker = track(value(
        r#"{"object":{"context":1,"base":2,"parent":3,"dirty":4,"target":5,"proxy":6}}"#,
    ));
    let prepared = settle(
        &mut tracker,
        r#"{"object":{"context":7,"base":2,"parent":3,"dirty":4,"target":5,"proxy":8}}"#,
    );
    let keys: Vec<_> = prepared.value()["object"]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.to_string())
        .collect();
    assert_eq!(
        keys,
        ["context", "base", "parent", "dirty", "target", "proxy"]
    );
}

#[test]
fn emits_deletion_and_supports_root_arrays() {
    let mut object = track(value(r#"{"keep":1,"remove":2}"#));
    let prepared = settle(&mut object, r#"{"keep":1}"#);
    assert_eq!(wire(prepared.ops()), value(r#"[["d",["remove"]]]"#));
    assert_eq!(**object.value(), value(r#"{"keep":1}"#));

    let mut array = track(value("[1,2,3]"));
    settle(&mut array, "[3,2,1,4]");
    assert_eq!(**array.value(), value("[3,2,1,4]"));
}

#[test]
fn preserves_native_object_key_order_after_delete_and_re_add() {
    // Key order is not observable through diff (object equality ignores order), so the
    // delete and re-add are supplied as an explicit batch.
    let mut tracker = track(value(r#"{"first":1,"second":2}"#));
    let mut change = tracker.begin_change();
    let keys = |v: &JsonValue| -> Vec<String> {
        v.as_object()
            .unwrap()
            .keys()
            .map(|k| k.to_string())
            .collect()
    };
    change
        .apply_ops(&[
            Op::Delete(vec!["first".into()]),
            Op::Set(vec!["first".into()], JsonValue::from(1)),
        ])
        .unwrap();
    assert_eq!(keys(change.current().unwrap()), ["second", "first"]);
    let prepared = change.prepare().unwrap();
    assert_eq!(keys(prepared.value()), ["second", "first"]);
    tracker.adopt(&prepared).unwrap();
    assert_eq!(keys(tracker.value()), ["second", "first"]);
}

#[test]
fn normalizes_deeply_equal_container_assignments_before_direct_candidate_materialization() {
    let initial = Arc::new(value(r#"{"child":{"a":1,"b":2},"count":0}"#));
    let mut tracker = Tracker::new(initial.clone());
    let prepared = prepare_next(&tracker, r#"{"child":{"b":2,"a":1},"count":1}"#);
    assert_eq!(wire(prepared.ops()), value(r#"[["s",["count"],1]]"#));
    let order = |v: &JsonValue| -> Vec<String> {
        v["child"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.to_string())
            .collect()
    };
    let replayed = apply_immutable(Some(&initial), prepared.ops()).unwrap();
    assert_eq!(order(prepared.value()), ["a", "b"]);
    assert_eq!(order(&replayed), ["a", "b"]);
    assert_eq!(**prepared.value(), replayed);
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn rejects_array_index_gaps_without_mutation_while_allowing_replacement_and_append() {
    let mut tracker = track(value(r#"{"values":[1,2]}"#));
    let mut change = tracker.begin_change();
    assert!(
        change
            .apply_ops(&[Op::Set(vec!["values".into(), 3.into()], JsonValue::from(4))])
            .is_err()
    );
    assert_eq!(change.current().unwrap(), &value(r#"{"values":[1,2]}"#));
    assert_eq!(**tracker.value(), value(r#"{"values":[1,2]}"#));
    change
        .apply_ops(&[
            Op::Set(vec!["values".into(), 1.into()], JsonValue::from(9)),
            Op::Set(vec!["values".into(), 2.into()], JsonValue::from(3)),
        ])
        .unwrap();
    let prepared = change.prepare().unwrap();
    assert_eq!(**prepared.value(), value(r#"{"values":[1,9,3]}"#));
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();
    assert_eq!(**tracker.value(), value(r#"{"values":[1,9,3]}"#));
}

#[test]
fn does_not_dirty_read_only_traversals() {
    let mut tracker = track(value(r#"{"nested":{"rows":[{"value":1}]}}"#));
    let change = tracker.begin_change();
    assert_eq!(
        change.current().unwrap()["nested"]["rows"][0]["value"],
        JsonValue::from(1)
    );
    let mut change = change;
    let prepared = change.prepare().unwrap();
    assert!(prepared.ops().is_empty());
    tracker.adopt(&prepared).unwrap();
}

// ---- delta tracker by-value placements (native-relevant cases) ----

#[test]
fn expands_repeated_source_aliases_into_independent_placements() {
    let mut tracker = track(value(r#"{"left":null,"right":null,"rows":[]}"#));
    let shared = value(r#"{"nested":{"value":1}}"#);
    let mut change = tracker.begin_change();
    let mut draft = change.current().unwrap().clone();
    {
        let object = draft.as_object_mut().unwrap();
        object.insert("left".into(), shared.clone());
        object.insert("right".into(), shared.clone());
        object.insert(
            "rows".into(),
            JsonValue::Array(vec![shared.clone(), shared].into()),
        );
    }
    *change.state().unwrap() = draft;
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.value()["left"], prepared.value()["right"]);
    // independent edits through the shared source do not leak between placements
    let mut edited = prepared.value().as_ref().clone();
    set_at(
        &mut edited,
        &["left".into(), "nested".into(), "value".into()],
        JsonValue::from(9),
    );
    set_at(
        &mut edited,
        &["rows".into(), 0.into(), "nested".into(), "value".into()],
        JsonValue::from(8),
    );
    tracker.adopt(&prepared).unwrap();
    let prepared = prepare_next(&tracker, &edited.to_string());
    assert_eq!(
        **prepared.value(),
        value(
            r#"{"left":{"nested":{"value":9}},"right":{"nested":{"value":1}},"rows":[{"nested":{"value":8}},{"nested":{"value":1}}]}"#
        )
    );
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
}

#[test]
fn deep_clones_draft_sourced_placements_within_and_across_revisions() {
    let mut tracker = track(value(
        r#"{"a":{"child":{"value":1}},"b":null,"rows":[{"child":{"value":1}},{"child":{"value":2}}]}"#,
    ));
    let first = settle(
        &mut tracker,
        r#"{"a":{"child":{"value":1}},"b":{"child":{"value":3}},"rows":[{"child":{"value":1}},{"child":{"value":1}}]}"#,
    );
    assert_eq!(first.value()["a"]["child"]["value"], JsonValue::from(1));
    assert_eq!(first.value()["b"]["child"]["value"], JsonValue::from(3));
    let second = settle(
        &mut tracker,
        r#"{"a":{"child":{"value":1}},"b":{"child":{"value":3}},"rows":[{"child":{"value":1}},{"child":{"value":4}}]}"#,
    );
    assert_eq!(
        wire(second.ops()),
        value(r#"[["s",["rows",1,"child","value"],4]]"#)
    );
}

#[test]
fn distinguishes_raw_committed_references_from_draft_references() {
    let mut tracker = track(value(
        r#"{"source":{"value":1},"rawCopy":null,"draftCopy":null}"#,
    ));
    let prepared = settle(
        &mut tracker,
        r#"{"source":{"value":2},"rawCopy":{"value":1},"draftCopy":{"value":2}}"#,
    );
    assert_eq!(
        **prepared.value(),
        value(r#"{"source":{"value":2},"rawCopy":{"value":1},"draftCopy":{"value":2}}"#)
    );
}

#[test]
fn folds_edits_to_introduced_object_and_array_subtrees_into_placement_payloads() {
    let mut tracker = track(value(r#"{"nested":null,"rows":[]}"#));
    let prepared = prepare_next(
        &tracker,
        r#"{"nested":{"rows":[{"value":2},{"value":3}]},"rows":[{"values":[1,2]}]}"#,
    );
    assert_eq!(
        wire(prepared.ops()),
        value(
            r#"[["s",["nested"],{"rows":[{"value":2},{"value":3}]}],["p",["rows"],0,0,[{"values":[1,2]}]]]"#
        )
    );
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn folds_pathological_operation_counts_without_payload_cost_estimation() {
    let wide = |n: i64| {
        let fields: Vec<String> = (0..5000).map(|i| format!(r#""field{i}":{n}"#)).collect();
        format!("{{{}}}", fields.join(","))
    };
    let mut tracker = track(value(&wide(0)));
    let prepared = prepare_next(&tracker, &wide(1));
    assert_eq!(prepared.ops().len(), 1);
    assert!(matches!(prepared.ops()[0], Op::Replace(_)));
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();

    let sparse = |edited: bool| {
        format!(
            r#"{{"rows":{}}}"#,
            rows(15000, |i| if edited && i % 3 == 0 {
                -(i as i64) - 1
            } else {
                i as i64
            })
        )
    };
    let mut tracker = track(value(&sparse(false)));
    let prepared = prepare_next(&tracker, &sparse(true));
    assert_eq!(prepared.ops().len(), 1);
    assert!(matches!(prepared.ops()[0], Op::Replace(_)));
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn shares_immutable_operation_placements_with_the_materialized_candidate() {
    let mut tracker = track(value(r#"{"rows":[]}"#));
    let prepared = prepare_next(&tracker, r#"{"rows":[{"value":1}]}"#);
    let operations = prepared.shared_ops();
    let Op::Splice(_, _, _, inserted) = &operations[0] else {
        panic!("expected splice")
    };
    assert!(std::ptr::eq(
        inserted[0].as_object().unwrap(),
        prepared.value()["rows"][0].as_object().unwrap()
    ));
    tracker.adopt(&prepared).unwrap();
    assert!(Arc::ptr_eq(tracker.value(), prepared.value()));
}

// ---- delta tracker piece arrays (native-relevant cases) ----

fn numbers(range: impl Iterator<Item = i64>) -> String {
    format!(
        "[{}]",
        range.map(|v| v.to_string()).collect::<Vec<_>>().join(",")
    )
}

#[test]
fn emits_one_insert_for_an_append_to_a_long_array() {
    let mut tracker = track(value(&format!(r#"{{"values":{}}}"#, numbers(0..10000))));
    let next = format!(r#"{{"values":{}}}"#, numbers((0..10000).chain([-1])));
    let prepared = settle(&mut tracker, &next);
    assert_eq!(
        wire(prepared.ops()),
        value(r#"[["p",["values"],10000,0,[-1]]]"#)
    );
    assert_eq!(tracker.value()["values"].as_array().unwrap().len(), 10001);
}

/// Op shape differs from upstream (recorded piece-array normalization is not ported); the
/// final value and replay contract still hold under diff mode.
#[test]
fn diff_of_a_shift_splice_pop_push_sequence_replays_to_the_final_value() {
    let mut tracker = track(value(r#"{"values":[0,1,2,3,4,5,6,7,8,9]}"#));
    let prepared = settle(&mut tracker, r#"{"values":[1,20,4,5,6,8,30]}"#);
    assert!(!prepared.ops().is_empty());
}

#[test]
fn emits_removals_from_the_end_backwards_then_inserts_when_kept_elements_keep_their_order() {
    let path = [Seg::from("values")];
    let mut tracker = track(value(r#"{"values":[0,1,2,3,4,5,6,7,8,9]}"#));
    let mut change = tracker.begin_change();
    assert_eq!(change.splice(&path, 0, 1, vec![]).unwrap(), [num(0)]); // shift
    assert_eq!(
        change.splice(&path, 1, 2, vec![num(20)]).unwrap(),
        [num(2), num(3)]
    );
    assert_eq!(change.splice(&path, 5, 1, vec![]).unwrap(), [num(7)]);
    assert_eq!(change.splice(&path, 6, 1, vec![]).unwrap(), [num(9)]); // pop
    assert!(
        change
            .splice(&path, 6, 0, vec![num(30)])
            .unwrap()
            .is_empty()
    ); // push
    assert_eq!(
        change.current().unwrap(),
        &value(r#"{"values":[1,20,4,5,6,8,30]}"#)
    );
    let prepared = change.prepare().unwrap();
    assert_eq!(
        wire(prepared.ops()),
        value(
            r#"[["p",["values"],9,1,[]],["p",["values"],7,1,[]],["p",["values"],2,2,[]],["p",["values"],0,1,[]],["p",["values"],1,0,[20]],["p",["values"],6,0,[30]]]"#
        )
    );
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();
    assert_eq!(**tracker.value(), value(r#"{"values":[1,20,4,5,6,8,30]}"#));
}

#[test]
fn emits_a_permutation_when_kept_elements_change_order() {
    // The draft `reverse`/`sort` mutators: `permute` records the reorder as pieces.
    for (order, expected) in [
        ([4, 3, 2, 1, 0], r#"{"values":[4,3,2,1,0]}"#),
        ([0, 2, 4, 1, 3], r#"{"values":[0,2,4,1,3]}"#),
    ] {
        let tracker = track(value(r#"{"values":[0,1,2,3,4]}"#));
        let mut change = tracker.begin_change();
        change.permute(&["values".into()], &order).unwrap();
        let prepared = change.prepare().unwrap();
        assert!(prepared.ops().iter().any(|op| matches!(op, Op::Move(..))));
        assert_eq!(
            apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
            value(expected)
        );
    }

    // Reverse is also detected by diff; explicit permutation batches are retained.
    let mut tracker = track(value(r#"{"values":[0,1,2,3,4]}"#));
    let prepared = settle(&mut tracker, r#"{"values":[4,3,2,1,0]}"#);
    assert!(prepared.ops().iter().any(|op| matches!(op, Op::Move(..))));

    let mut tracker = track(value(r#"{"values":[0,1,2,3,4]}"#));
    let mut change = tracker.begin_change();
    let ops = [Op::Move(vec!["values".into()], vec![0, 2, 4, 1, 3])];
    change.apply_ops(&ops).unwrap();
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.ops(), ops);
    assert_eq!(**prepared.value(), value(r#"{"values":[0,2,4,1,3]}"#));
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn keeps_adoption_independent_from_mutable_detached_permutation_metadata() {
    let mut tracker = track(value(r#"{"values":[3,1,2]}"#));
    let prepared = prepare_next(&tracker, r#"{"values":[1,2,3]}"#);
    let mut detached = prepared.ops().to_vec();
    let Some(Op::Move(_, permutation)) = detached.iter_mut().find(|op| matches!(op, Op::Move(..)))
    else {
        panic!("expected permutation")
    };
    permutation.reverse();
    tracker.adopt(&prepared).unwrap();
    assert_eq!(**tracker.value(), value(r#"{"values":[1,2,3]}"#));
}

#[test]
fn combines_movement_and_edits_with_permutation_plus_final_paths() {
    // Sort through the draft mutator, then edit the held element at its final index.
    let initial =
        r#"{"values":[{"rank":3,"edited":0},{"rank":1,"edited":0},{"rank":2,"edited":0}]}"#;
    let mut tracker = track(value(initial));
    let mut change = tracker.begin_change();
    change.permute(&["values".into()], &[1, 2, 0]).unwrap();
    set_at(
        change.state().unwrap(),
        &["values".into(), 2.into(), "edited".into()],
        JsonValue::from(1),
    );
    let prepared = change.prepare().unwrap();
    assert_eq!(
        wire(&prepared.ops()[..1]),
        value(r#"[["m",["values"],[1,2,0]]]"#)
    );
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();

    // An explicit batch is retained verbatim.
    let mut tracker = track(value(initial));
    let mut change = tracker.begin_change();
    let ops = [
        Op::Move(vec!["values".into()], vec![1, 2, 0]),
        Op::Set(
            vec!["values".into(), 2.into(), "edited".into()],
            JsonValue::from(1),
        ),
    ];
    change.apply_ops(&ops).unwrap();
    let prepared = change.prepare().unwrap();
    assert_eq!(
        wire(&prepared.ops()[..1]),
        value(r#"[["m",["values"],[1,2,0]]]"#)
    );
    assert_eq!(prepared.ops(), ops);
    assert_eq!(
        **prepared.value(),
        value(r#"{"values":[{"rank":1,"edited":0},{"rank":2,"edited":0},{"rank":3,"edited":1}]}"#)
    );
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn folds_dense_child_and_direct_index_edits_into_one_full_array_operation() {
    // Edits are made in place on the draft, as upstream's `row.value += 1`.
    let mut tracker = track(value(&format!(
        r#"{{"rows":{}}}"#,
        rows(1000, |i| i as i64)
    )));
    let mut change = tracker.begin_change();
    for row in field(change.state().unwrap(), "rows")
        .as_array_mut()
        .unwrap()
    {
        let value = field(row, "value");
        *value = num(value.as_f64().unwrap() as i64 + 1);
    }
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.ops().len(), 1);
    assert!(matches!(prepared.ops()[0], Op::Splice(..)));
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();

    let mut tracker = track(value(&format!(r#"{{"values":{}}}"#, numbers(0..1000))));
    let mut change = tracker.begin_change();
    let values = field(change.state().unwrap(), "values")
        .as_array_mut()
        .unwrap();
    for (index, value) in values.iter_mut().take(600).enumerate() {
        *value = num(-(index as i64) - 1);
    }
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.ops().len(), 1);
    assert!(matches!(prepared.ops()[0], Op::Splice(..)));
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();
}

fn nested_rows(count: usize, field: impl Fn(usize) -> i64) -> String {
    let items: Vec<String> = (0..count)
        .map(|i| format!(r#"{{"nested":{{"value":{}}}}}"#, field(i)))
        .collect();
    format!(r#"{{"rows":[{}]}}"#, items.join(","))
}

#[test]
fn folds_only_a_deeply_nested_dense_array_region() {
    let mut tracker = track(value(&nested_rows(2000, |i| i as i64)));
    let prepared = prepare_next(
        &tracker,
        &nested_rows(2000, |i| match i {
            0 => -1,
            500..1000 => -(i as i64),
            _ => i as i64,
        }),
    );
    let splice = prepared
        .ops()
        .iter()
        .find_map(|op| match op {
            Op::Splice(path, at, count, inserted) => Some((path, *at, *count, inserted.len())),
            _ => None,
        })
        .expect("regional splice");
    assert_eq!((splice.1, splice.2, splice.3), (500, 500, 500));
    assert_eq!(prepared.ops().len(), 2);
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn does_not_cache_emission_paths_for_nested_reserved_key_folds_covered_by_an_outer_region() {
    let build = |edited: bool| {
        let items: Vec<String> = (0..400)
            .map(|i| {
                let flag = i64::from(edited && i < 256);
                if i == 100 {
                    let values: Vec<String> = (0..400)
                        .map(|v| {
                            if edited && v < 256 {
                                -(v as i64) - 1
                            } else {
                                v as i64
                            }
                            .to_string()
                        })
                        .collect();
                    format!(
                        r#"{{"flag":{flag},"special":{{"__proto__":{{"values":[{}]}}}}}}"#,
                        values.join(",")
                    )
                } else {
                    format!(r#"{{"flag":{flag}}}"#)
                }
            })
            .collect();
        format!(r#"{{"rows":[{}]}}"#, items.join(","))
    };
    let mut tracker = track(value(&build(false)));
    let prepared = prepare_next(&tracker, &build(true));
    assert_eq!(prepared.ops().len(), 1);
    let Op::Splice(path, at, count, _) = &prepared.ops()[0] else {
        panic!("expected splice")
    };
    assert_eq!((path.len(), *at, *count), (1, 0, 256));
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn suppresses_nested_structural_and_leaf_operations_covered_by_an_outer_dense_region() {
    let build = |edited: bool| {
        let items: Vec<String> = (0..400)
            .map(|i| {
                let flag = i64::from(edited && i < 256);
                let values = if i == 100 {
                    let mut items: Vec<String> = (0..400)
                        .map(|v| {
                            let n = if edited && v < 256 {
                                -(v as i64) - 1
                            } else {
                                v as i64
                            };
                            format!(r#"{{"value":{n}}}"#)
                        })
                        .collect();
                    if edited {
                        items.push(r#"{"value":999}"#.into());
                    }
                    items.join(",")
                } else {
                    String::new()
                };
                format!(r#"{{"flag":{flag},"values":[{values}]}}"#)
            })
            .collect();
        format!(r#"{{"rows":[{}]}}"#, items.join(","))
    };
    let mut tracker = track(value(&build(false)));
    let prepared = prepare_next(&tracker, &build(true));
    assert_eq!(prepared.ops().len(), 1);
    let Op::Splice(_, at, count, _) = &prepared.ops()[0] else {
        panic!("expected splice")
    };
    assert_eq!((*at, *count), (0, 256));
    assert_eq!(
        prepared.value()["rows"][100]["values"]
            .as_array()
            .unwrap()
            .len(),
        401
    );
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn emits_multiple_disjoint_dense_regions_and_preserves_operations_outside_their_boundaries() {
    let negative = |i: usize| -(i as i64) - 1;
    let mut tracker = track(value(&format!(
        r#"{{"values":{}}}"#,
        rows(1400, |i| i as i64)
    )));
    let prepared = prepare_next(
        &tracker,
        &format!(
            r#"{{"values":{}}}"#,
            rows(1400, |i| match i {
                97 | 358 | 897 | 1158 | 100..356 | 900..1156 => negative(i),
                _ => i as i64,
            })
        ),
    );
    let splices: Vec<_> = prepared
        .ops()
        .iter()
        .filter_map(|op| match op {
            Op::Splice(_, at, count, _) => Some((*at, *count)),
            _ => None,
        })
        .collect();
    assert_eq!(splices, [(100, 256), (900, 256)]);
    for index in [97usize, 358, 897, 1158] {
        assert!(prepared.ops().contains(&Op::Set(
            vec!["values".into(), index.into(), "value".into()],
            num(negative(index))
        )));
    }
    assert_eq!(prepared.ops().len(), 6);
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn normalizes_a_20000_operation_queue_transaction_to_two_operations() {
    let mut tracker = track(value(&format!(
        r#"{{"values":{}}}"#,
        rows(20000, |i| i as i64)
    )));
    let path = [Seg::from("values")];
    let mut change = tracker.begin_change();
    for index in 0..10_000 {
        change.splice(&path, 0, 1, vec![]).unwrap(); // shift
        let row = value(&format!(r#"{{"value":{}}}"#, 20_000 + index));
        change.splice(&path, 19_999, 0, vec![row]).unwrap(); // push
    }
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.ops().len(), 2);
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn handles_adversarial_fragmentation_by_final_value_replay() {
    // The held-handle resolution half of the upstream case is draft-proxy-only.
    let size = 20000usize;
    let held = [1usize, 1001, 5001, 10001, 15001, 19999];
    let mut tracker = track(value(&format!(
        r#"{{"values":{}}}"#,
        rows(size, |i| i as i64)
    )));
    let prepared = prepare_next(
        &tracker,
        &format!(
            r#"{{"values":{}}}"#,
            rows(size, |i| match i {
                _ if i % 2 == 0 => -(i as i64) - 1,
                _ if held.contains(&i) => i as i64 + 100000,
                _ => i as i64,
            })
        ),
    );
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn normalizes_restored_overrides_and_cancelled_structural_edits_to_no_operations() {
    let mut tracker = track(value(r#"{"values":[1,2,3]}"#));
    let prepared = prepare_next(&tracker, r#"{"values":[1,2,3]}"#);
    assert!(prepared.ops().is_empty());
    assert_eq!(**prepared.value(), value(r#"{"values":[1,2,3]}"#));
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn handles_null_sparse_overrides_without_confusing_absence() {
    let mut tracker = track(value(&format!(r#"{{"values":{}}}"#, numbers(0..10000))));
    let next = format!(
        r#"{{"values":[{}]}}"#,
        (0..10000)
            .map(|i| if i == 17 || i == 9000 {
                "null".to_string()
            } else {
                i.to_string()
            })
            .collect::<Vec<_>>()
            .join(",")
    );
    settle(&mut tracker, &next);
    assert!(tracker.value()["values"][17].is_null());
    assert!(tracker.value()["values"][9000].is_null());
}

#[test]
fn inserts_100000_items_with_unshift_and_splice() {
    for method in ["unshift", "splice"] {
        let mut tracker = track(value(r#"{"values":[-1]}"#));
        let items = numbers(0..100000);
        let inner = &items[1..items.len() - 1];
        let next = if method == "unshift" {
            format!(r#"{{"values":[{inner},-1]}}"#)
        } else {
            format!(r#"{{"values":[-1,{inner}]}}"#)
        };
        let prepared = settle(&mut tracker, &next);
        let values = prepared.value()["values"].as_array().unwrap();
        assert_eq!(values.len(), 100001);
        let at = if method == "unshift" { 99999 } else { 100000 };
        assert_eq!(values[at], JsonValue::from(99999));
    }
}

// ---- delta tracker randomized transactions ----

struct Rng(u32);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6d2b_79f5);
        let mut value = (self.0 ^ (self.0 >> 15)).wrapping_mul(1 | self.0);
        value = value.wrapping_add((value ^ (value >> 7)).wrapping_mul(61 | value)) ^ value;
        f64::from(value ^ (value >> 14)) / 4_294_967_296.0
    }
}
#[derive(Clone)]
struct Document {
    values: Vec<(i64, i64)>,
    text: String,
    revision: i64,
    label: Option<String>,
}
impl Document {
    fn render(&self) -> String {
        let values: Vec<String> = self
            .values
            .iter()
            .map(|(id, score)| format!(r#"{{"id":{id},"score":{score}}}"#))
            .collect();
        let label = self
            .label
            .as_ref()
            .map(|l| format!(r#","label":"{l}""#))
            .unwrap_or_default();
        format!(
            r#"{{"values":[{}],"text":"{}","meta":{{"revision":{}{label}}}}}"#,
            values.join(","),
            self.text,
            self.revision
        )
    }
    fn mutate(&mut self, choice: usize, v: i64) {
        let item = (v, v % 7);
        match choice {
            0 => self.text.push_str(&format!("-{v}")),
            1 => self.values.push(item),
            2 => self.values.insert(0, item),
            3 if !self.values.is_empty() => {
                self.values.remove(0);
            }
            4 => {
                self.values.pop();
            }
            5 => {
                let len = self.values.len();
                let index = if len == 0 { 0 } else { v as usize % (len + 1) };
                let remove = if len == 0 {
                    0
                } else {
                    (v as usize % 2).min(len - index)
                };
                self.values.splice(index..index + remove, [item]);
            }
            6 => self.values.reverse(),
            7 => self.values.sort_by_key(|(id, _)| *id),
            8 if !self.values.is_empty() => {
                let len = self.values.len();
                self.values[v as usize % len].1 = v;
            }
            9 => {
                self.revision += 1;
                self.label = Some(format!("r-{v}"));
            }
            10 => self.label = None,
            _ => {}
        }
    }
}

#[test]
fn matches_policy_detached_replay_and_adopted_state_after_multi_operation_transactions() {
    for seed in 1..=40u32 {
        let mut rng = Rng(seed);
        let mut expected = Document {
            values: (0..4).map(|id| (id, 0)).collect(),
            text: "start".into(),
            revision: 0,
            label: None,
        };
        let mut tracker = track(value(&expected.render()));
        for transaction in 0..25u32 {
            let base = tracker.value().clone();
            let snapshot = base.as_ref().clone();
            let mut change = tracker.begin_change();
            for operation in 0..5u32 {
                let choice = (rng.next() * 11.0) as usize;
                let v = i64::from(seed * 10_000 + transaction * 10 + operation);
                expected.mutate(choice, v);
            }
            *change.state().unwrap() = value(&expected.render());
            let prepared = change.prepare().unwrap();
            let context = format!("seed {seed} transaction {transaction}");
            assert_eq!(
                **prepared.value(),
                value(&expected.render()),
                "policy {context}"
            );
            assert_eq!(*base, snapshot, "prepare changed base {context}");
            assert_eq!(
                apply_immutable(Some(&base), prepared.ops()).unwrap(),
                **prepared.value(),
                "replay {context}"
            );
            tracker.adopt(&prepared).unwrap();
            assert_eq!(*base, snapshot, "adopt changed base {context}");
            assert!(Arc::ptr_eq(tracker.value(), prepared.value()), "{context}");
        }
    }
}

// ---- delta tracker object emission scaling / security ----

#[test]
fn orders_many_reverse_depth_object_edits_through_the_bounded_fallback() {
    // Built structurally: the strict JSON parser caps nesting well below this depth.
    let chain = |edit: bool| {
        let mut node: Option<JsonValue> = None;
        for level in (0..=512).rev() {
            let mut map = pi_chord::json::Map::new();
            map.insert("value".into(), num(if edit { level + 1 } else { 0 }));
            if let Some(next) = node.take() {
                map.insert("next".into(), next);
            }
            node = Some(JsonValue::Object(map.into()));
        }
        node.unwrap()
    };
    let mut tracker = track(chain(false));
    let mut change = tracker.begin_change();
    *change.state().unwrap() = chain(true);
    let prepared = change.prepare().unwrap();
    // One ordered leaf edit per node: tracker emission has no snapshot-size fallback.
    assert_eq!(prepared.ops().len(), 513);
    assert!(prepared.ops().iter().all(|op| matches!(op, Op::Set(..))));
    assert_eq!(
        apply_immutable(Some(tracker.value()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn handles_reserved_own_keys_without_prototype_pollution() {
    let mut tracker = track(value(r#"{"safe":{"__proto__":{"value":1}}}"#));
    let prepared = settle(&mut tracker, r#"{"safe":{"__proto__":{"value":2}}}"#);
    assert_eq!(
        prepared.value()["safe"]["__proto__"]["value"],
        JsonValue::from(2)
    );
    assert_eq!(tracker.value().as_object().unwrap().len(), 1);
}

#[test]
fn supports_a_memory_storage_style_recursive_clone_of_prepared_value() {
    let tracker = track(value(r#"{"rows":[{"value":1}]}"#));
    let prepared = prepare_next(&tracker, r#"{"rows":[{"value":2},{"value":3}]}"#);
    let cloned: JsonValue = serde_json::from_str(&prepared.value().to_string()).unwrap();
    assert_eq!(cloned, value(r#"{"rows":[{"value":2},{"value":3}]}"#));
}

// ---- delta tracker retention across GC jobs (ownership via Weak) ----

fn big_root(rows_count: usize) -> Arc<JsonValue> {
    Arc::new(value(&format!(
        r#"{{"payload":{{"rows":{}}}}}"#,
        rows(rows_count, |i| i as i64)
    )))
}
fn small_root(v: i64) -> Arc<JsonValue> {
    Arc::new(value(&format!(
        r#"{{"payload":{{"rows":[{{"value":{v}}}]}}}}"#
    )))
}

#[test]
fn validates_aborted_payload_retention_semantics() {
    let tracker = track(value(r#"{"payload":null}"#));
    let mut change = tracker.begin_change();
    *change.state().unwrap() = (*big_root(50000)).clone();
    change.abort();
    assert!(change.current().is_err());
    assert_eq!(**tracker.value(), value(r#"{"payload":null}"#));
}

#[test]
fn validates_unadopted_prepared_retention_semantics() {
    let tracker = track(value(r#"{"payload":null}"#));
    let mut change = tracker.begin_change();
    *change.state().unwrap() = (*big_root(50000)).clone();
    let prepared = change.prepare().unwrap();
    let weak = Arc::downgrade(prepared.value());
    assert!(weak.upgrade().is_some());
    drop(prepared);
    drop(change);
    assert!(weak.upgrade().is_none());
    assert_eq!(**tracker.value(), value(r#"{"payload":null}"#));
}

#[test]
fn validates_settled_lifecycle_retention_semantics() {
    let mut tracker = track(value(r#"{"payload":null}"#));
    let mut change = tracker.begin_change();
    *change.state().unwrap() = (*small_root(1)).clone();
    let retained = change.prepare().unwrap();
    tracker.adopt(&retained).unwrap();
    let future = tracker.prepare_replace(big_root(50000));
    tracker.adopt(&future).unwrap();
    let tracker_root = Arc::downgrade(tracker.value());
    let future_root = Arc::downgrade(future.value());
    drop(tracker);
    drop(future);
    // the retained settled change and prepared must not keep the future roots alive
    assert!(tracker_root.upgrade().is_none());
    assert!(future_root.upgrade().is_none());
    drop((change, retained));
}

#[test]
fn validates_obsolete_revisions_retention_semantics() {
    let mut tracker = Tracker::new(big_root(50000));
    let old_root = Arc::downgrade(tracker.value());
    for v in 0..100 {
        let prepared = tracker.prepare_replace(small_root(v));
        tracker.adopt(&prepared).unwrap();
    }
    assert!(old_root.upgrade().is_none());
    assert_eq!(**tracker.value(), *small_root(99));
}

#[test]
fn validates_retained_settled_prepared_retention_semantics() {
    let mut tracker = Tracker::new(big_root(50000));
    let mut change = tracker.begin_change();
    let mut draft = tracker.value().as_ref().clone();
    set_at(
        &mut draft,
        &["payload".into(), "rows".into(), 0.into(), "value".into()],
        num(-1),
    );
    *change.state().unwrap() = draft;
    let prepared = change.prepare().unwrap();
    let base = Arc::downgrade(prepared.base());
    let current = Arc::downgrade(prepared.value());
    tracker.adopt(&prepared).unwrap();
    {
        let replacement = tracker.prepare_replace(small_root(1));
        tracker.adopt(&replacement).unwrap();
    }
    // a retained Prepared retains its immutable revisions
    assert!(base.upgrade().is_some());
    assert!(current.upgrade().is_some());
    drop(prepared);
    assert!(base.upgrade().is_none());
    assert!(current.upgrade().is_none());
}

#[test]
fn validates_retained_settled_change_retention_semantics() {
    let mut tracker = Tracker::new(big_root(50000));
    let mut change = tracker.begin_change();
    let mut draft = tracker.value().as_ref().clone();
    set_at(
        &mut draft,
        &["payload".into(), "rows".into(), 0.into(), "value".into()],
        num(-1),
    );
    *change.state().unwrap() = draft;
    let prepared = change.prepare().unwrap();
    let base = Arc::downgrade(prepared.base());
    let current = Arc::downgrade(prepared.value());
    tracker.adopt(&prepared).unwrap();
    drop(prepared);
    {
        let replacement = tracker.prepare_replace(small_root(1));
        tracker.adopt(&replacement).unwrap();
    }
    // the settled Change is still held but must not retain either revision
    assert!(base.upgrade().is_none());
    assert!(current.upgrade().is_none());
    assert!(change.current().is_err());
}

#[test]
fn validates_stale_unprepared_change_retention_semantics() {
    let mut tracker = Tracker::new(big_root(50000));
    let mut stale = tracker.begin_change();
    set_at(
        stale.state().unwrap(),
        &["payload".into(), "rows".into(), 0.into(), "value".into()],
        num(-1),
    );
    let obsolete = Arc::downgrade(tracker.value());
    {
        let winner = tracker.prepare_replace(small_root(1));
        tracker.adopt(&winner).unwrap();
    }
    assert!(stale.prepare().is_err());
    // the stale change is retained but must have released the obsolete revision
    assert!(obsolete.upgrade().is_none());
    assert!(stale.current().is_err());
}

#[test]
fn validates_lifecycle_churn_retention_semantics() {
    let mut tracker = track(value(r#"{"value":0}"#));
    for index in 0..100_000 {
        let mut change = tracker.begin_change();
        *change.state().unwrap() = value(&format!(r#"{{"value":{index}}}"#));
        change.abort();
    }
    let prepared = tracker.prepare_replace(Arc::new(value(r#"{"value":1}"#)));
    tracker.adopt(&prepared).unwrap();
    assert_eq!(**tracker.value(), value(r#"{"value":1}"#));
}
