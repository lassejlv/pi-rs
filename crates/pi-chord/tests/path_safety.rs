//! Direct tests of the path-safety and base-detection primitives, with the inputs of
//! upstream's `delta.test.ts` ("supports root replacement…", "rejects unsafe and malformed paths").
use pi_chord::delta::*;
use pi_chord::json::Value;
macro_rules! json { ($($tokens:tt)+) => { Value::from(serde_json::json!($($tokens)+)) }; }

#[test]
fn reserved_segments_are_exactly_upstreams_set() {
    assert_eq!(RESERVED_SEGMENTS, ["__proto__", "constructor", "prototype"]);
}

#[test]
fn assert_safe_path_rejects_each_reserved_key_at_any_depth_and_accepts_the_rest() {
    for reserved in RESERVED_SEGMENTS {
        for path in [
            vec![Seg::from(reserved)],
            vec![Seg::from("a"), Seg::from(reserved)],
            vec![Seg::from(reserved), Seg::from(0usize)],
        ] {
            match assert_safe_path(&path) {
                Err(Error::UnsafePath(segment)) => assert_eq!(segment, reserved),
                other => panic!("{path:?}: {other:?}"),
            }
        }
    }
    assert!(assert_safe_path(&[]).is_ok());
    assert!(
        assert_safe_path(&[
            Seg::from("values"),
            Seg::from(3usize),
            Seg::from("__proto__x")
        ])
        .is_ok()
    );
    // Keys that merely contain a reserved word, or differ in case, are ordinary keys.
    assert!(assert_safe_path(&[Seg::from("Constructor"), Seg::from("proto")]).is_ok());
}

#[test]
fn applying_through_reserved_or_out_of_range_segments_is_an_unsafe_path() {
    let set = |path: Vec<Seg>| vec![Op::Set(path, json!(true))];
    let err = apply(
        json!({}),
        &set(vec!["constructor".into(), "prototype".into(), "x".into()]),
    )
    .unwrap_err();
    assert!(matches!(err, Error::UnsafePath(_)), "{err:?}");
    // An index may address an element or append exactly one past the end.
    let err = apply(
        json!({ "values": [1] }),
        &[Op::Set(vec!["values".into(), 3usize.into()], json!(2))],
    )
    .unwrap_err();
    assert!(matches!(err, Error::UnsafePath(_)), "{err:?}");
    let appended = apply(
        json!({ "values": [1] }),
        &[Op::Set(vec!["values".into(), 1usize.into()], json!(2))],
    )
    .unwrap();
    assert_eq!(appended, json!({ "values": [1, 2] }));
}

#[test]
fn is_replace_and_is_base_follow_the_first_operation() {
    let base = vec![Op::Replace(json!([1, 2, 3]))];
    assert!(is_replace(&base[0]));
    assert!(is_base(&base));
    // Root replacement, root splice and permutation, as upstream's test applies them.
    let mut value = apply(Value::Null, &base).unwrap();
    value = apply(value, &[Op::Splice(vec![], 1, 1, vec![json!(4)])]).unwrap();
    value = apply(value, &[Op::Move(vec![], vec![2, 0, 1])]).unwrap();
    assert_eq!(value, json!([3, 1, 4]));
    for not_base in [
        vec![],
        vec![Op::Set(vec!["a".into()], json!(1))],
        vec![Op::Delete(vec!["a".into()]), Op::Replace(json!(1))],
        vec![Op::Splice(vec![], 0, 0, vec![])],
    ] {
        assert!(!is_base(&not_base), "{not_base:?}");
    }
    assert!(!is_replace(&Op::Truncate(vec![], 0)));
    assert!(!is_replace(&Op::Append(vec!["t".into()], "x".into())));
}
