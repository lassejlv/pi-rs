use pi_chord::delta::*;
use pi_chord::{
    JsonValue,
    json::{parse, to_value},
};
use std::sync::Arc;
fn value(s: &str) -> JsonValue {
    parse(s).unwrap()
}

#[test]
fn whole_draft_updates_reuse_unchanged_native_containers() {
    let initial = value(r#"{"items":[1,2],"nested":{"count":0},"other":{"label":"keep"}}"#);
    let tracker = track(initial.clone());
    let mut change = tracker.begin_change();
    *change.state().unwrap() =
        value(r#"{"items":[1,2],"nested":{"count":1},"other":{"label":"keep"}}"#);
    let prepared = change.prepare().unwrap();
    assert!(std::ptr::eq(
        initial["items"].as_array().unwrap(),
        prepared.value()["items"].as_array().unwrap()
    ));
    assert!(std::ptr::eq(
        initial["other"].as_object().unwrap(),
        prepared.value()["other"].as_object().unwrap()
    ));
    assert!(!std::ptr::eq(
        initial["nested"].as_object().unwrap(),
        prepared.value()["nested"].as_object().unwrap()
    ));
}

#[test]
fn prepared_set_payload_is_the_adopted_native_container() {
    let mut tracker = track(value(r#"{"other":{},"items":[1]}"#));
    let mut change = tracker.begin_change();
    let payload = value(r#"{"label":"new"}"#);
    change
        .apply_ops(&[Op::Set(vec!["other".into()], payload.clone())])
        .unwrap();
    let prepared = change.prepare().unwrap();
    let first = prepared.shared_ops();
    let second = prepared.shared_ops();
    assert!(Arc::ptr_eq(&first, &second));
    let Op::Set(_, stored) = &first[0] else {
        panic!("expected a Set")
    };
    assert!(std::ptr::eq(
        payload.as_object().unwrap(),
        stored.as_object().unwrap()
    ));
    tracker.adopt(&prepared).unwrap();
    assert!(std::ptr::eq(
        stored.as_object().unwrap(),
        tracker.value()["other"].as_object().unwrap()
    ));
}

#[tokio::test]
async fn drafts_survive_await_and_authority_changes_only_after_adoption() {
    let initial = Arc::new(value(r#"{"count":1,"nested":{"text":"a"},"values":[1]}"#));
    let mut tracker = Tracker::new(initial.clone());
    let mut change = tracker.begin_change();
    *change.state().unwrap() = value(r#"{"count":2,"nested":{"text":"ab"},"values":[1,2]}"#);
    tokio::task::yield_now().await;
    assert!(Arc::ptr_eq(tracker.value(), &initial));
    let prepared = change.prepare().unwrap();
    assert_eq!(
        to_value(prepared.ops()).unwrap(),
        value(r#"[["s",["count"],2],["a",["nested","text"],"b"],["p",["values"],1,0,[2]]]"#)
    );
    assert!(change.state().is_err());
    assert!(Arc::ptr_eq(prepared.base(), tracker.value()));
    assert_eq!(
        apply_immutable(Some(prepared.base()), prepared.ops()).unwrap(),
        **prepared.value()
    );
    tracker.adopt(&prepared).unwrap();
    assert!(Arc::ptr_eq(prepared.value(), tracker.value()));
    assert_eq!(tracker.revision(), 1);
}

#[test]
fn failed_persistence_can_abort_candidate_without_publishing_it() {
    let initial = Arc::new(value(r#"{"value":1}"#));
    let mut tracker = Tracker::new(initial.clone());
    let mut change = tracker.begin_change();
    *change.state().unwrap() = value(r#"{"value":2}"#);
    let candidate = change.prepare().unwrap();
    change.abort();
    change.abort();
    assert!(
        tracker
            .adopt(&candidate)
            .unwrap_err()
            .to_string()
            .contains("aborted")
    );
    assert!(Arc::ptr_eq(tracker.value(), &initial));
    assert_eq!(tracker.revision(), 0);
    assert_eq!(**candidate.value(), value(r#"{"value":2}"#));
}

#[test]
fn foreign_stale_reused_and_aborted_candidates_are_rejected() {
    let mut first = track(value(r#"{"value":0}"#));
    let mut second = track(value(r#"{"value":0}"#));
    let mut change = first.begin_change();
    let mut competing = first.begin_change();
    let mut open_competing = first.begin_change();
    *change.state().unwrap() = value(r#"{"value":1}"#);
    *competing.state().unwrap() = value(r#"{"value":2}"#);
    let winner = change.prepare().unwrap();
    let loser = competing.prepare().unwrap();
    assert!(
        second
            .adopt(&winner)
            .unwrap_err()
            .to_string()
            .contains("different tracker")
    );
    first.adopt(&winner).unwrap();
    assert!(
        first
            .adopt(&winner)
            .unwrap_err()
            .to_string()
            .contains("already been used")
    );
    for _ in 0..2 {
        assert!(
            first
                .adopt(&loser)
                .unwrap_err()
                .to_string()
                .contains("stale")
        );
    }
    assert!(open_competing.state().is_err());
    assert!(open_competing.prepare().is_err());
    let aborted = first.prepare_replace(Arc::new(value(r#"{"value":3}"#)));
    aborted.abort();
    assert!(
        first
            .adopt(&aborted)
            .unwrap_err()
            .to_string()
            .contains("aborted")
    );
}

#[test]
fn no_op_adoption_retains_identity_and_stales_competitors() {
    let initial = Arc::new(value(r#"{"nested":{"a":1,"b":2}}"#));
    let mut tracker = Tracker::new(initial.clone());
    let mut change = tracker.begin_change();
    *change.state().unwrap() = value(r#"{"nested":{"b":2,"a":1}}"#);
    let first = change.prepare().unwrap();
    let competitor = tracker.prepare_replace(initial.clone());
    assert!(first.ops().is_empty());
    assert!(Arc::ptr_eq(first.value(), &initial));
    tracker.adopt(&first).unwrap();
    assert!(Arc::ptr_eq(tracker.value(), &initial));
    assert_eq!(tracker.revision(), 1);
    assert!(
        tracker
            .adopt(&competitor)
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
}

#[test]
fn replacement_is_a_base_and_takes_candidate_ownership() {
    let mut tracker = track(value(r#"{"value":1}"#));
    let replacement = Arc::new(value(r#"{"value":2}"#));
    let candidate = tracker.prepare_replace(replacement.clone());
    assert!(Arc::ptr_eq(candidate.value(), &replacement));
    assert!(is_base(candidate.ops()));
    assert_eq!(candidate.base_revision(), 0);
    tracker.adopt(&candidate).unwrap();
    let no_op = tracker.prepare_replace(Arc::new(value(r#"{"value":2}"#)));
    assert!(no_op.ops().is_empty());
    assert!(Arc::ptr_eq(no_op.base(), no_op.value()));
}

#[test]
fn revision_diff_matches_pinned_reference_cases_and_replays_both_directions() {
    let fixture: JsonValue = serde_json::from_str(include_str!("fixtures/delta.json")).unwrap();
    for row in fixture["diffs"].as_array().unwrap() {
        let native = diff_revisions(&row["before"], &row["after"]);
        assert_eq!(to_value(&native).unwrap(), row["ops"]);
        assert_eq!(
            apply_immutable(Some(&row["before"]), &native).unwrap(),
            row["after"]
        );
        let reference: Vec<Op> = serde_json::from_str(&row["ops"].to_string()).unwrap();
        assert_eq!(
            apply_immutable(Some(&row["before"]), &reference).unwrap(),
            row["after"]
        );
    }
}

#[test]
fn large_array_edits_remain_narrow_and_rotation_is_payload_free() {
    let a: Vec<_> = (0..40000)
        .map(|i| {
            JsonValue::Object(
                [
                    (String::from("value").into(), JsonValue::from(i)),
                    (
                        String::from("stable").into(),
                        JsonValue::from("retained row payload"),
                    ),
                ]
                .into_iter()
                .collect::<pi_chord::json::Map>()
                .into(),
            )
        })
        .collect();
    let mut b = a.clone();
    b[20000] = JsonValue::Null;
    let ops = diff_revisions(
        &JsonValue::Array(a.clone().into()),
        &JsonValue::Array(b.clone().into()),
    );
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Op::Set(..)));
    assert_eq!(
        apply(JsonValue::Array(a.clone().into()), &ops).unwrap(),
        JsonValue::Array(b.into())
    );
    let mut rotated = a.clone();
    rotated.rotate_left(1000);
    let ops = diff_revisions(
        &JsonValue::Array(a.clone().into()),
        &JsonValue::Array(rotated.clone().into()),
    );
    assert!(matches!(&ops[..], [Op::Move(..)]));
    assert_eq!(
        apply(JsonValue::Array(a.into()), &ops).unwrap(),
        JsonValue::Array(rotated.into())
    );
}

#[test]
fn wide_changes_fall_back_to_one_base_and_reserved_paths_are_folded() {
    let before = JsonValue::Object(
        (0..10000)
            .map(|i| (format!("key{i}").into(), JsonValue::from(i)))
            .collect::<pi_chord::json::Map>()
            .into(),
    );
    let after = JsonValue::Object(
        (0..10000)
            .map(|i| (format!("key{i}").into(), JsonValue::Null))
            .collect::<pi_chord::json::Map>()
            .into(),
    );
    let ops = diff_revisions(&before, &after);
    assert_eq!(ops, vec![Op::Replace(after)]);
    let before = value(r#"{"safe":{"__proto__":{"a":1}}}"#);
    let after = value(r#"{"safe":{"__proto__":{"a":2}}}"#);
    let ops = diff_revisions(&before, &after);
    assert!(matches!(&ops[..],[Op::Set(path,_)] if path==&vec![Seg::from("safe")]));
    assert_eq!(apply(before, &ops).unwrap(), after);
}

#[test]
fn recorded_structural_edits_survive_equal_final_values_and_publish_only_at_adoption() {
    let initial = Arc::new(value(r#"{"items":[1,2]}"#));
    let mut tracker = Tracker::new(initial.clone());
    let mut change = tracker.begin_change();
    let ops: Vec<Op> =
        serde_json::from_str(r#"[["p",["items"],0,1,[]],["p",["items"],0,0,[1]]]"#).unwrap();
    change.apply_ops(&ops).unwrap();
    assert_eq!(change.current().unwrap(), initial.as_ref());
    assert!(Arc::ptr_eq(tracker.value(), &initial));
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.ops(), ops);
    assert!(!Arc::ptr_eq(prepared.value(), &initial));
    assert_eq!(
        apply_immutable(Some(&initial), prepared.ops()).unwrap(),
        **prepared.value()
    );
    assert!(change.current().is_err());
    assert!(change.apply_ops(&ops).is_err());
    tracker.adopt(&prepared).unwrap();
    assert!(Arc::ptr_eq(tracker.value(), prepared.value()));
}

#[test]
fn recorded_edits_mix_with_opaque_updates_and_reject_failed_candidates_atomically() {
    let mut tracker = track(value(r#"{"a":0,"b":0,"items":[]}"#));
    let mut change = tracker.begin_change();
    *change.state().unwrap() = value(r#"{"a":1,"b":0,"items":[]}"#);
    let ops = vec![Op::Splice(
        vec![Seg::from("items")],
        0,
        0,
        vec![JsonValue::from(3)],
    )];
    change.apply_ops(&ops).unwrap();
    *change.state().unwrap() = value(r#"{"a":1,"b":2,"items":[3]}"#);
    let before = change.current().unwrap().clone();
    assert!(
        change
            .apply_ops(&[Op::Append(vec![Seg::from("a")], "bad".into())])
            .is_err()
    );
    assert!(
        change
            .apply_ops_checked(&[Op::Replace(JsonValue::Null)], |_| Err(Error::Type(
                "rejected".into()
            )))
            .is_err()
    );
    assert_eq!(change.current().unwrap(), &before);
    change
        .apply_ops(&[Op::Set(vec![Seg::from("a")], JsonValue::from(4))])
        .unwrap();
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.ops().len(), 4);
    assert_eq!(
        apply_immutable(Some(prepared.base()), prepared.ops()).unwrap(),
        value(r#"{"a":4,"b":2,"items":[3]}"#)
    );
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn retained_settled_changes_release_their_old_roots() {
    for abort in [false, true] {
        let mut tracker = track(value(r#"{"retained":"old root"}"#));
        let weak = Arc::downgrade(tracker.value());
        let mut change = tracker.begin_change();
        *change.state().unwrap() = value(r#"{"retained":"new root"}"#);
        if abort {
            change.abort();
            drop(tracker);
        } else {
            let prepared = change.prepare().unwrap();
            tracker.adopt(&prepared).unwrap();
            drop(prepared);
        }
        assert!(weak.upgrade().is_none());
        assert!(change.current().is_err());
    }
}

#[test]
fn recorded_replacements_discard_overwritten_prefixes_and_remain_classifiable_as_bases() {
    let mut tracker = track(value(r#"{"count":0}"#));
    let mut change = tracker.begin_change();
    change
        .apply_ops(&[Op::Set(vec!["count".into()], JsonValue::from(1))])
        .unwrap();
    let replacement = value(r#"{"count":2}"#);
    change
        .apply_ops(&[
            Op::Replace(replacement.clone()),
            Op::Set(vec!["count".into()], JsonValue::from(3)),
        ])
        .unwrap();
    let prepared = change.prepare().unwrap();
    assert!(is_base(prepared.ops()));
    assert_eq!(
        prepared.ops(),
        &[
            Op::Replace(replacement),
            Op::Set(vec!["count".into()], JsonValue::from(3))
        ]
    );
    assert_eq!(
        apply_immutable(None, prepared.ops()).unwrap(),
        value(r#"{"count":3}"#)
    );
    tracker.adopt(&prepared).unwrap();
}
