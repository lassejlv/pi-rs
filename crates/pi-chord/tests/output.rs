use pi_chord::{JsonString, JsonValue, json::to_value, output::*};
fn fixture() -> JsonValue {
    serde_json::from_str(include_str!("fixtures/output.json")).unwrap()
}
fn limits(value: &JsonValue) -> OutputLimits {
    serde_json::from_str(&value.to_string()).unwrap()
}

#[test]
fn pinned_reference_bounds_sanitization_and_byte_counts() {
    for (index, row) in fixture()["bounded"].as_array().unwrap().iter().enumerate() {
        let text = row["text"].as_string().unwrap();
        assert_eq!(
            to_value(&bound_output(text, limits(&row["limits"]))).unwrap(),
            row["result"],
            "bounds {index}"
        );
        assert_eq!(
            JsonValue::String(sanitize_output(text)),
            row["sanitized"],
            "sanitize {index}"
        );
        assert_eq!(
            utf8_byte_length(text),
            row["byteLength"].as_u64().unwrap() as usize,
            "byte length {index}"
        );
    }
}

#[test]
fn pinned_reference_chunked_output_and_exact_dropped_totals() {
    for (index, row) in fixture()["streams"].as_array().unwrap().iter().enumerate() {
        let mut buffer = OutputBuffer::new(limits(&row["limits"]));
        for (step_index, step) in row["steps"].as_array().unwrap().iter().enumerate() {
            let skipped = if step["skipped"].is_null() {
                None
            } else {
                Some(serde_json::from_str(&step["skipped"].to_string()).unwrap())
            };
            let accepted = if step["kind"].string_equals("text") {
                buffer.push_text(step["data"].as_string().unwrap().clone(), skipped)
            } else {
                let bytes: Vec<_> = step["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap() as u8)
                    .collect();
                buffer.push_bytes(&bytes, skipped)
            }
            .unwrap();
            assert_eq!(
                accepted,
                step["accepted"].as_bool().unwrap(),
                "accepted stream {index} step {step_index}"
            );
            assert_eq!(
                to_value(&buffer.snapshot()).unwrap(),
                step["result"],
                "snapshot stream {index} step {step_index}"
            );
            assert_eq!(
                buffer.stored_bytes(),
                step["storedBytes"].as_u64().unwrap() as usize,
                "stored stream {index} step {step_index}"
            );
        }
        buffer.end();
        assert_eq!(
            to_value(&buffer.snapshot()).unwrap(),
            row["result"],
            "final snapshot stream {index}"
        );
        assert_eq!(
            buffer.stored_bytes(),
            row["storedBytes"].as_u64().unwrap() as usize,
            "final stored stream {index}"
        );
    }
}

#[test]
fn skipped_output_requires_tail_retention() {
    let mut buffer = OutputBuffer::new(OutputLimits {
        max_bytes: 100,
        max_lines: 10,
        retain: Retain::Head,
    });
    assert_eq!(
        buffer
            .push_text(
                "text",
                Some(OutputSkip {
                    bytes: 1000,
                    newlines: 100,
                    ends_with_newline: true
                })
            )
            .unwrap_err(),
        "Skipped output requires tail retention"
    );
}

#[test]
fn tail_storage_stays_bounded_across_a_long_stream() {
    let mut buffer = OutputBuffer::new(OutputLimits {
        max_bytes: 1024,
        max_lines: 10,
        retain: Retain::Tail,
    });
    let chunk = JsonString::from("retained line of output\n");
    for _ in 0..10000 {
        buffer.push_text(chunk.clone(), None).unwrap();
        assert!(buffer.stored_bytes() <= 1024 + utf8_byte_length(&chunk) * 2);
    }
    let result = buffer.snapshot();
    assert_eq!(result.dropped_lines, 9990);
}

// Ports of the upstream `test/output.test.ts` and `test/output-skip.test.ts` cases.

fn head(max_lines: usize, max_bytes: usize) -> OutputLimits {
    OutputLimits {
        max_bytes,
        max_lines,
        retain: Retain::Head,
    }
}
fn tail(max_lines: usize, max_bytes: usize) -> OutputLimits {
    OutputLimits {
        max_bytes,
        max_lines,
        retain: Retain::Tail,
    }
}
fn text(value: &str) -> JsonString {
    JsonString::from(value)
}
fn units(value: &[u16]) -> JsonString {
    JsonString::from_utf16(value.to_vec())
}
fn bound(value: &str, limits: OutputLimits) -> (String, usize, usize) {
    let kept = bound_output(&text(value), limits);
    (
        kept.text.to_string_lossy(),
        kept.dropped_bytes,
        kept.dropped_lines,
    )
}
fn output(value: &str, dropped_bytes: usize, dropped_lines: usize) -> BoundedOutput {
    BoundedOutput {
        text: text(value),
        dropped_bytes,
        dropped_lines,
    }
}

/// The tail of `units` as Node's `Buffer` slices it: on a character boundary.
fn buffer_tail(input: &[u16], max_bytes: usize) -> JsonString {
    let bytes = String::from_utf16_lossy(input).into_bytes();
    if bytes.len() <= max_bytes {
        return units(input);
    }
    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && bytes[start] & 0xc0 == 0x80 {
        start += 1;
    }
    JsonString::from(String::from_utf8(bytes[start..].to_vec()).unwrap())
}
fn assert_matches_buffer_tail(input: &[u16], max_byte_values: Option<&[usize]>) {
    let total_bytes = String::from_utf16_lossy(input).len();
    let all: Vec<usize> = (0..total_bytes + 5).collect();
    for &max_bytes in max_byte_values.unwrap_or(&all) {
        let kept = bound_output(&units(input), tail(10, max_bytes)).text;
        let expected = buffer_tail(input, max_bytes);
        assert_eq!(
            kept.units(),
            expected.units(),
            "tail mismatch input={input:04x?} max_bytes={max_bytes}"
        );
        assert!(
            utf8_byte_length(&kept) <= max_bytes || total_bytes <= max_bytes,
            "tail output exceeded {max_bytes} bytes"
        );
    }
}
fn sampled_byte_limits(input: &[u16]) -> Vec<usize> {
    let total = String::from_utf16_lossy(input).len() as i64;
    let mut values: Vec<usize> = [
        0,
        1,
        2,
        3,
        4,
        5,
        8,
        total / 2,
        total - 4,
        total - 1,
        total,
        total + 1,
    ]
    .into_iter()
    .filter(|v| *v >= 0)
    .map(|v| v as usize)
    .collect();
    values.sort_unstable();
    values.dedup();
    values
}

#[test]
fn tool_output_bounds_removes_control_characters_but_keeps_tabs_newlines_and_other_text() {
    assert_eq!(
        sanitize_output(&text("a\0b\tc\nd\re\u{7}f\u{fff9}g\u{fffb}h😀")),
        text("ab\tc\ndefgh😀")
    );
}

#[test]
fn tool_output_bounds_keeps_output_within_the_limits_unchanged() {
    assert_eq!(bound("a\nb\n", head(2, 1000)), ("a\nb\n".into(), 0, 0));
    assert_eq!(bound("a\nb", tail(2, 1000)), ("a\nb".into(), 0, 0));
    assert_eq!(bound("", tail(2, 1000)), ("".into(), 0, 0));
}

#[test]
fn tool_output_bounds_keeps_nothing_with_a_zero_limit() {
    assert_eq!(bound("ab\ncd\n", head(10, 0)), ("".into(), 6, 2));
    assert_eq!(bound("ab\ncd\n", tail(0, 1000)), ("".into(), 6, 2));
}

#[test]
fn tool_output_bounds_keeps_exact_slices_of_whole_lines_trailing_newline_included() {
    assert_eq!(bound("a\nb\nc\n", head(2, 1000)), ("a\nb\n".into(), 2, 1));
    assert_eq!(bound("a\nb\nc\n", tail(2, 1000)), ("b\nc\n".into(), 2, 1));
    assert_eq!(bound("a\nb\nc", tail(2, 1000)), ("b\nc".into(), 2, 1));
    // Blank lines are lines.
    assert_eq!(
        bound("a\nb\nc\n\n", tail(3, 1000)),
        ("b\nc\n\n".into(), 2, 1)
    );
}

#[test]
fn tool_output_bounds_cuts_at_the_byte_limit_on_whole_lines_when_possible() {
    assert_eq!(
        bound("aa\nbb\ncc\n", head(10, 7)),
        ("aa\nbb\n".into(), 3, 1)
    );
    assert_eq!(
        bound("aa\nbb\ncc\n", tail(10, 7)),
        ("bb\ncc\n".into(), 3, 1)
    );
}

#[test]
fn tool_output_bounds_cuts_a_single_line_longer_than_the_byte_limit_on_a_character_boundary() {
    // "é" is two bytes; five bytes hold two whole characters.
    assert_eq!(bound("ééé\n", head(10, 5)), ("éé".into(), 3, 0));
    assert_eq!(bound("x\néééé", tail(10, 5)), ("éé".into(), 6, 1));
}

#[test]
fn tool_output_bounds_keeps_a_u_feff_at_the_start_of_a_kept_slice() {
    assert_eq!(bound("x\u{feff}a", tail(10, 4)), ("\u{feff}a".into(), 1, 0));
}

#[test]
fn tool_output_bounds_cuts_tails_of_surrogate_edge_cases_exactly_like_buffer() {
    let inputs: [&[u16]; 6] = [
        &[0x61, 0xd83d],
        &[0xde42, 0x62],
        &[0x61, 0xde42, 0x62],
        &[0xd83d, 0xd83d, 0xde42],
        &[0xd83d, 0xde42, 0xde42],
        &[0xd83d, 0xdc69, 0x200d, 0xd83d, 0xdcbb],
    ];
    for input in inputs {
        assert_matches_buffer_tail(input, None);
    }
}

#[test]
fn tool_output_bounds_cuts_tails_exactly_like_buffer_across_deterministic_fuzz_cases() {
    let alphabet: [&[u16]; 15] = [
        &[0x61],
        &[0x7f],
        &[0x80],
        &[0xe9],
        &[0x7ff],
        &[0x800],
        &[0x4e2d],
        &[0xd7ff],
        &[0xd800],
        &[0xd83d],
        &[0xdc00],
        &[0xde42],
        &[0xd83d, 0xde42],
        &[0xe000],
        &[0xffff],
    ];
    fn exhaustive(prefix: &mut Vec<u16>, depth: usize, alphabet: &[&[u16]]) {
        assert_matches_buffer_tail(prefix, Some(&sampled_byte_limits(prefix)));
        if depth == 0 {
            return;
        }
        for character in alphabet {
            let length = prefix.len();
            prefix.extend_from_slice(character);
            exhaustive(prefix, depth - 1, alphabet);
            prefix.truncate(length);
        }
    }
    exhaustive(&mut Vec::new(), 3, &alphabet);
    let mut seed: u32 = 0x1234_5678;
    let mut random = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(seed) / 4_294_967_296.0
    };
    for _ in 0..1_000 {
        let mut input = Vec::new();
        let length = (random() * 80.0).floor() as usize;
        for _ in 0..length {
            input.extend_from_slice(alphabet[(random() * alphabet.len() as f64).floor() as usize]);
        }
        assert_matches_buffer_tail(&input, Some(&sampled_byte_limits(&input)));
    }
}

#[test]
fn output_buffer_keeps_exact_totals_across_chunks_and_decodes_utf_8_split_across_byte_chunks() {
    let mut buffer = OutputBuffer::new(tail(2, 1000));
    let bytes = "😀\n".as_bytes();
    buffer.push_text("a\nb\n", None).unwrap();
    buffer.push_bytes(&bytes[..2], None).unwrap();
    buffer.push_bytes(&bytes[2..], None).unwrap();
    assert_eq!(buffer.snapshot(), output("b\n😀\n", 2, 1));
}

#[test]
fn output_buffer_sanitizes_the_retained_text_but_counts_the_raw_stream() {
    let mut buffer = OutputBuffer::new(tail(1, 1000));
    buffer.push_text("a\u{7}\n", None).unwrap();
    assert_eq!(buffer.snapshot(), output("a\n", 0, 0));
    buffer.push_text("b\u{1b}\n", None).unwrap();
    assert_eq!(buffer.snapshot(), output("b\n", 3, 1));
}

#[test]
fn output_buffer_drops_a_byte_order_mark_only_at_the_start_of_the_output() {
    let bom = [0xef, 0xbb, 0xbf];
    let mut buffer = OutputBuffer::new(tail(10, 1000));
    buffer.push_bytes(&bom[..1], None).unwrap();
    buffer.push_bytes(&[0xbb, 0xbf, 0x61], None).unwrap();
    buffer.push_text("b", None).unwrap();
    // A U+FEFF after a string chunk is text.
    buffer
        .push_bytes(&[bom[0], bom[1], bom[2], 0x63], None)
        .unwrap();
    buffer.end();
    assert_eq!(buffer.snapshot().text, text("ab\u{feff}c"));
}

#[test]
fn output_buffer_flushes_an_incomplete_character_before_a_string_chunk_and_at_the_end() {
    let mut buffer = OutputBuffer::new(tail(10, 1000));
    let euro = "€".as_bytes();
    buffer.push_bytes(&euro[..1], None).unwrap();
    buffer.push_text("x", None).unwrap();
    buffer.push_bytes(&euro[..2], None).unwrap();
    buffer.end();
    assert_eq!(buffer.snapshot().text, text("\u{fffd}x\u{fffd}"));
}

#[test]
fn output_buffer_matches_bounding_the_whole_stream_when_several_chunks_arrive_between_snapshots() {
    for limits in [head(3, 40), tail(3, 40), head(50, 25), tail(50, 25)] {
        let mut buffer = OutputBuffer::new(limits);
        let mut stream = String::new();
        for index in 0..300 {
            let chunk = if index % 7 == 0 {
                format!("{}\n", "é".repeat(index % 30))
            } else {
                format!("line {index}\n")
            };
            stream.push_str(&chunk);
            buffer.push_text(chunk.as_str(), None).unwrap();
            if index % 5 != 4 {
                continue;
            }
            let expected = bound_output(&text(&stream), limits);
            assert_eq!(
                buffer.snapshot(),
                BoundedOutput {
                    text: expected.text,
                    dropped_bytes: expected.dropped_bytes,
                    dropped_lines: expected.dropped_lines,
                },
                "{limits:?} chunk {index}"
            );
        }
    }
}

#[test]
fn output_buffer_stops_storing_head_output_once_the_window_is_full() {
    let mut buffer = OutputBuffer::new(head(2, 1000));
    for index in 0..1000 {
        buffer
            .push_text(format!("line {index}\n").as_str(), None)
            .unwrap();
    }
    assert!(buffer.stored_bytes() < 20);
    assert_eq!(buffer.snapshot(), output("line 0\nline 1\n", 8876, 998));
}

#[test]
fn output_buffer_stores_only_the_tail_window_after_each_snapshot() {
    let mut buffer = OutputBuffer::new(tail(3, 100));
    let mut stream = String::new();
    for index in 0..2000 {
        let chunk = format!("line {index}\n\n");
        stream.push_str(&chunk);
        buffer.push_text(chunk.as_str(), None).unwrap();
        let snapshot = buffer.snapshot();
        assert!(buffer.stored_bytes() <= 100, "chunk {index}");
        assert_eq!(
            snapshot.text,
            bound_output(&text(&stream), tail(3, 100)).text,
            "chunk {index}"
        );
    }
}

/// Deterministic PRNG (mulberry32) so failures reproduce from the printed seed.
fn random(seed: u32) -> impl FnMut() -> f64 {
    let mut state = seed;
    move || {
        state = state.wrapping_add(0x6d2b_79f5);
        let mut t = state;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

// Decoded shell output: ASCII, multi-byte and astral characters, replacement characters, CR/LF, tabs, and control
// characters that sanitizing removes after bounding.
const ALPHABET: [&str; 15] = [
    "a", "b", "z", " ", "\n", "\n", "\n", "\r\n", "\t", "é", "€", "😀", "\u{fffd}", "\x01", "\x1b",
];

fn random_chunk(next: &mut impl FnMut() -> f64) -> String {
    let length = (next() * 12.0).floor() as usize;
    (0..length)
        .map(|_| ALPHABET[(next() * ALPHABET.len() as f64).floor() as usize])
        .collect()
}
fn measure(value: &str) -> OutputSkip {
    OutputSkip {
        bytes: value.len(),
        newlines: value.matches('\n').count(),
        ends_with_newline: value.ends_with('\n'),
    }
}
/// Whether `value` proves that everything before it is outside the tail window.
fn exceeds_window(value: &str, limits: OutputLimits) -> bool {
    let skip = measure(value);
    skip.bytes > limits.max_bytes || skip.newlines > limits.max_lines
}
fn random_limits(next: &mut impl FnMut() -> f64) -> OutputLimits {
    let max_bytes = 1 + (next() * 40.0).floor() as usize;
    let max_lines = 1 + (next() * 5.0).floor() as usize;
    tail(max_lines, max_bytes)
}

/// Feed random output to one buffer in full and to another through a reference skipper that follows the contract of
/// `ShellOutputInfo.skipped`.
fn check_seed(seed: u32) {
    let mut next = random(seed);
    let limits = random_limits(&mut next);
    let mut full = OutputBuffer::new(limits);
    let mut skipping = OutputBuffer::new(limits);
    let mut pending = String::new();
    let flush = |pending: &mut String,
                 skipping: &mut OutputBuffer,
                 full: &mut OutputBuffer,
                 next: &mut dyn FnMut() -> f64| {
        if pending.is_empty() {
            return;
        }
        let cuts: Vec<usize> = pending
            .char_indices()
            .map(|(offset, _)| offset)
            .chain([pending.len()])
            .filter(|cut| *cut > 0 && exceeds_window(&pending[*cut..], limits))
            .collect();
        if !cuts.is_empty() && next() < 0.7 {
            let cut = cuts[(next() * cuts.len() as f64).floor() as usize];
            skipping
                .push_text(&pending[cut..], Some(measure(&pending[..cut])))
                .unwrap();
        } else {
            skipping.push_text(pending.as_str(), None).unwrap();
        }
        pending.clear();
        // Progress snapshots happen between deliveries and compact stored chunks.
        if next() < 0.5 {
            skipping.snapshot();
        }
        assert_eq!(skipping.snapshot(), full.snapshot(), "seed {seed}");
    };
    let chunks = (next() * 40.0).floor() as usize;
    for _ in 0..chunks {
        let chunk = random_chunk(&mut next);
        full.push_text(chunk.as_str(), None).unwrap();
        if next() < 0.3 {
            full.snapshot();
        }
        pending.push_str(&chunk);
        if next() < 0.3 {
            flush(&mut pending, &mut skipping, &mut full, &mut next);
        }
    }
    flush(&mut pending, &mut skipping, &mut full, &mut next);
    full.end();
    skipping.end();
    assert_eq!(skipping.snapshot(), full.snapshot(), "seed {seed}");
}

// Progress commits snapshot at arbitrary moments; snapshots compact what is stored and must never change the window.
#[test]
fn output_buffer_skipped_output_keeps_the_same_tail_whenever_progress_snapshots_happen() {
    for seed in 1..=3000 {
        let mut next = random(seed);
        let limits = random_limits(&mut next);
        let mut plain = OutputBuffer::new(limits);
        let mut sampled = OutputBuffer::new(limits);
        let chunks = (next() * 30.0).floor() as usize;
        for _ in 0..chunks {
            let chunk = random_chunk(&mut next);
            plain.push_text(chunk.as_str(), None).unwrap();
            sampled.push_text(chunk.as_str(), None).unwrap();
            if next() < 0.4 {
                sampled.snapshot();
            }
        }
        assert_eq!(sampled.snapshot(), plain.snapshot(), "seed {seed}");
    }
}

#[test]
fn output_buffer_skipped_output_matches_the_full_stream_for_every_legal_skip_pattern() {
    for seed in 1..=3000 {
        check_seed(seed);
    }
}

#[test]
fn output_buffer_skipped_output_counts_skipped_bytes_lines_and_the_final_newline_exactly() {
    let limits = tail(2, 1000);
    let mut full = OutputBuffer::new(limits);
    let mut skipping = OutputBuffer::new(limits);
    // The skipped text ends without a newline, so its last line continues in the delivered text.
    let omitted = "one\ntwo\nthr";
    let kept = "ee\nfour\nfive\nsix";
    full.push_text(format!("{omitted}{kept}").as_str(), None)
        .unwrap();
    skipping.push_text(kept, Some(measure(omitted))).unwrap();
    assert_eq!(skipping.snapshot(), full.snapshot());
    assert_eq!(skipping.snapshot(), output("five\nsix", 19, 4));
}

#[test]
fn output_buffer_skipped_output_refuses_skips_for_head_retention() {
    let mut buffer = OutputBuffer::new(OutputLimits {
        max_bytes: 10,
        max_lines: 2,
        retain: Retain::Head,
    });
    let error = buffer
        .push_text(
            "x\ny\nz\n",
            Some(OutputSkip {
                bytes: 3,
                newlines: 1,
                ends_with_newline: true,
            }),
        )
        .unwrap_err();
    assert!(error.contains("tail retention"), "{error}");
}
