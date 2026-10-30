use pi_codemode::parse_codemode_source_utf16;

#[test]
fn options_removal_preserves_source_units_and_line_numbers() {
    let mut input: Vec<u16> = "// @options: {\"timeout_ms\":7,\"max_output_tokens\":2}\r\nreturn '"
        .encode_utf16()
        .collect();
    input.push(0xd800);
    input.extend("';".encode_utf16());
    let parsed = parse_codemode_source_utf16(&input).unwrap();
    let expected: Vec<u16> = "\nreturn '"
        .encode_utf16()
        .chain([0xd800])
        .chain("';".encode_utf16())
        .collect();
    assert_eq!(parsed.code.units(), expected);
    assert_eq!(parsed.options.timeout_ms, Some(7));
    assert_eq!(parsed.options.max_output_tokens, Some(2));
}

#[test]
fn invalid_option_names_retain_raw_surrogates_in_the_error() {
    let input: Vec<u16> = "// @options: {\""
        .encode_utf16()
        .chain([0xd800])
        .chain("\":1}\nreturn 1;".encode_utf16())
        .collect();
    let error = parse_codemode_source_utf16(&input).unwrap_err();
    assert_eq!(error.name(), "CodemodeSourceError");
    let expected: Vec<u16> = "@options only supports `max_output_tokens` and `timeout_ms`; got `"
        .encode_utf16()
        .chain([0xd800])
        .chain("`".encode_utf16())
        .collect();
    assert_eq!(error.message.units(), expected);
}

#[test]
fn misplaced_directives_are_checked_after_raw_surrogate_comments() {
    let input: Vec<u16> = "//"
        .encode_utf16()
        .chain([0xdc00])
        .chain("\n // @options: {}\nreturn 1;".encode_utf16())
        .collect();
    let error = parse_codemode_source_utf16(&input).unwrap_err();
    assert_eq!(
        error.message.to_string_checked().unwrap(),
        "The // @options: line must be the first line of the input; found one on line 2"
    );
}
