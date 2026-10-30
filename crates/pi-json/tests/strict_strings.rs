//! String escaping and parsing keep exact UTF-16 content across the ASCII fast paths and the general path.
use pi_json::strict::{Value, parse, stringify};

fn string(units: Vec<u16>) -> Value {
    Value::String(pi_json::strict::JsonString::from_utf16(units))
}

#[test]
fn stringify_and_parse_round_trip_every_kind_of_unit() {
    let mut units: Vec<u16> =
        "plain ascii, \"quotes\", back\\slash, \n\r\t\u{8}\u{c}\u{1}\u{1f}, ~ and \u{7f}"
            .encode_utf16()
            .collect();
    units.extend("é中🙂".encode_utf16());
    units.push(0xd800); // lone high surrogate
    units.extend("tail".encode_utf16());
    units.push(0xdc00); // lone low surrogate
    units.extend("🙂".encode_utf16());
    units.push(0xd83d); // lone high surrogate at the end
    let value = string(units.clone());
    let text = stringify(&value);
    assert!(text.contains("\\ud800") && text.contains("\\udc00") && text.contains("\\ud83d"));
    assert!(
        text.contains("\\u0001")
            && text.contains("\\u001f")
            && text.contains("\\n")
            && text.contains("\\\\")
    );
    match parse(&text).unwrap() {
        Value::String(parsed) => assert_eq!(parsed.units(), units.as_slice()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn parse_handles_long_plain_runs_between_escapes_and_multibyte_characters() {
    let text = format!(
        r#""{}\n{}é{}A""#,
        "a".repeat(5000),
        "b".repeat(3000),
        "c".repeat(10)
    );
    let Value::String(parsed) = parse(&text).unwrap() else {
        panic!()
    };
    let expected: Vec<u16> = format!(
        "{}\n{}é{}A",
        "a".repeat(5000),
        "b".repeat(3000),
        "c".repeat(10)
    )
    .encode_utf16()
    .collect();
    assert_eq!(parsed.units(), expected.as_slice());
    assert!(
        parse("\"a\u{1}b\"").is_err(),
        "raw control characters stay invalid"
    );
    assert!(parse(r#""abc"#).is_err());
}
