//! Direct tests of the public tool, scan and decoding helpers. Expected values were produced by running
//! the upstream TypeScript functions (`--conditions=source`, pi-mono 5e63241) on the same inputs; the
//! decoder and line-scanner properties are upstream's `env-line-scan.test.ts` with its own generator.
use pi_durable::{
    Context, JsonString, ScanOrder,
    env::{
        FileSystem, LineScanner, NativeFileSystem,
        decode::{StreamDecoder, starts_with_bom},
    },
    storage::scan::{next_cursor, scan_start},
    tools::{
        edit_diff::{
            LineEnding, TextReplacement, apply_replacements_preserving_unchanged_lines,
            detect_line_ending, normalize_to_lf, restore_line_endings, strip_bom,
        },
        image::{detect_supported_image_mime_type, detect_supported_image_mime_type_of},
        image_processor::inline_image_type,
        path_utils::{normalize_tool_path, resolve_read_tool_path, resolve_tool_path},
    },
    truncate::{
        DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationOptions, truncate_head, truncate_head_of,
    },
};
use std::path::Path;

fn js(text: &str) -> JsonString {
    JsonString::from(text)
}

// ---- edit-diff.ts ----

#[test]
fn line_endings_bom_and_normalization_match_upstream() {
    assert_eq!(detect_line_ending(&js("a\r\nb")), LineEnding::Crlf);
    assert_eq!(detect_line_ending(&js("a\nb\r\n")), LineEnding::Lf);
    assert_eq!(detect_line_ending(&js("\nx")), LineEnding::Lf);
    assert_eq!(detect_line_ending(&js("abc")), LineEnding::Lf);
    // A lone CR is not a line ending for detection, but normalization folds it.
    assert_eq!(
        normalize_to_lf(&js("a\r\nb\rc\nd")).to_string_lossy(),
        "a\nb\nc\nd"
    );
    assert_eq!(
        restore_line_endings(&js("a\nb\n"), LineEnding::Crlf).to_string_lossy(),
        "a\r\nb\r\n"
    );
    assert_eq!(
        restore_line_endings(&js("a\nb\n"), LineEnding::Lf).to_string_lossy(),
        "a\nb\n"
    );
    let (bom, text) = strip_bom(&js("\u{feff}hello"));
    assert_eq!(
        (
            bom.to_string_lossy().as_ref(),
            text.to_string_lossy().as_ref()
        ),
        ("\u{feff}", "hello")
    );
    let (bom, text) = strip_bom(&js("hello"));
    assert_eq!(
        (
            bom.to_string_lossy().as_ref(),
            text.to_string_lossy().as_ref()
        ),
        ("", "hello")
    );
}

#[test]
fn replacements_keep_unchanged_lines_exactly_and_reject_different_line_counts() {
    let at = |match_index, new_text: &str| TextReplacement {
        match_index,
        match_length: 1,
        new_text: js(new_text),
    };
    let apply = |original: &str, base: &str, replacements: &[TextReplacement]| {
        apply_replacements_preserving_unchanged_lines(&js(original), &js(base), replacements)
            .map(|text| text.to_string_lossy())
    };
    // Unchanged lines keep their CRLF; the replaced line takes the base's LF.
    assert_eq!(
        apply("a\r\nb\r\nc\r\n", "a\nb\nc\n", &[at(2, "B")]).unwrap(),
        "a\r\nB\nc\r\n"
    );
    assert_eq!(
        apply("a\r\nb\r\nc\r\n", "a\nb\nc\n", &[at(0, "A"), at(4, "C")]).unwrap(),
        "A\nb\r\nC\n"
    );
    assert_eq!(
        apply("a\r\nb", "a\nb\nc\n", &[]).unwrap_err().to_string(),
        "Cannot preserve unchanged lines because the base content has a different line count."
    );
}

// ---- path-utils.ts ----

#[tokio::test]
async fn tool_paths_normalize_unicode_spaces_and_resolve_against_the_cwd() {
    assert_eq!(
        normalize_tool_path("@dir/a\u{a0}b\u{2003}c\u{202f}d\u{3000}e.txt"),
        "dir/a b c d e.txt"
    );
    assert_eq!(normalize_tool_path("no@prefix"), "no@prefix");
    let directory = tempfile::tempdir().unwrap();
    let fs = NativeFileSystem::new(directory.path()).unwrap();
    let context = Context::default();
    let resolved = resolve_tool_path(&fs, "@sub/f\u{a0}x.txt", &context)
        .await
        .unwrap();
    assert_eq!(resolved, fs.cwd().join("sub").join("f x.txt"));
    // Reads find a file whose name uses a narrow no-break space before AM/PM, as macOS screenshots do.
    std::fs::write(directory.path().join("Shot 10.00\u{202f}AM.png"), b"x").unwrap();
    let found = resolve_read_tool_path(&fs, "Shot 10.00 AM.png", &context)
        .await
        .unwrap();
    assert_eq!(
        found.file_name().unwrap().to_string_lossy(),
        "Shot 10.00\u{202f}AM.png"
    );
    // Smart quotes in the stored name are found from a straight-quote request.
    std::fs::write(directory.path().join("it\u{2019}s.txt"), b"x").unwrap();
    let found = resolve_read_tool_path(&fs, "it's.txt", &context)
        .await
        .unwrap();
    assert_eq!(
        found.file_name().unwrap().to_string_lossy(),
        "it\u{2019}s.txt"
    );
    // A missing file resolves to the plain path.
    let missing = resolve_read_tool_path(&fs, "none.txt", &context)
        .await
        .unwrap();
    assert_eq!(missing, fs.cwd().join("none.txt"));
}

// ---- truncate.ts ----

#[test]
fn truncation_defaults_and_prefix_totals_match_upstream() {
    assert_eq!((DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES), (2000, 51200));
    let limits = |lines: f64, bytes: f64| TruncationOptions {
        max_lines: Some(lines),
        max_bytes: Some(bytes),
    };
    let r = truncate_head_of(&js("one\ntwo\nthree\nfour"), 4.0, 18.0, limits(2.0, 100.0));
    assert_eq!(r.content.to_string_lossy(), "one\ntwo");
    assert_eq!(
        (r.truncated, r.truncated_by, r.total_lines, r.total_bytes),
        (true, Some("lines"), 4.0, 18.0)
    );
    assert_eq!(
        (
            r.output_lines,
            r.output_bytes,
            r.last_line_partial,
            r.first_line_exceeds_limit
        ),
        (2, 7, false, false)
    );
    let r = truncate_head_of(&js("one\ntwo\nthree\nfour"), 4.0, 18.0, limits(10.0, 100.0));
    assert_eq!(
        (
            r.content.to_string_lossy().as_ref(),
            r.truncated,
            r.truncated_by
        ),
        ("one\ntwo\nthree\nfour", false, None)
    );
    let r = truncate_head_of(&js("abcdefghij\nxy"), 2.0, 13.0, limits(10.0, 5.0));
    assert_eq!(
        (
            r.content.to_string_lossy().as_ref(),
            r.truncated_by,
            r.output_lines,
            r.first_line_exceeds_limit
        ),
        ("", Some("bytes"), 0, true)
    );
    let r = truncate_head_of(&js("aé🙂\nb"), 2.0, 9.0, limits(10.0, 7.0));
    assert_eq!(
        (
            r.content.to_string_lossy().as_ref(),
            r.truncated_by,
            r.output_lines,
            r.output_bytes
        ),
        ("aé🙂", Some("bytes"), 1, 7)
    );
    let r = truncate_head(&js("a\nb\nc"), TruncationOptions::default());
    assert_eq!(
        (
            r.truncated,
            r.total_lines,
            r.total_bytes,
            r.max_lines,
            r.max_bytes
        ),
        (false, 3.0, 5.0, 2000.0, 51200.0)
    );
}

// ---- storage/scan.ts ----

#[test]
fn scan_start_and_next_cursor_follow_upstream() {
    let cursor = |after: serde_json::Value, order: Option<&str>| {
        let mut value = serde_json::json!({ "after": after });
        if let Some(order) = order {
            value["order"] = order.into();
        }
        serde_json::from_value::<pi_durable::Cursor>(value).unwrap()
    };
    let start = |requested, cursor: Option<&pi_durable::Cursor>, fallback| {
        scan_start(requested, cursor, fallback)
    };
    use ScanOrder::{Ascending, Descending};
    let s = start(None, None, Ascending).unwrap();
    assert_eq!((s.order, s.after), (Ascending, None));
    let s = start(Some(Descending), None, Ascending).unwrap();
    assert_eq!((s.order, s.after), (Descending, None));
    let c = cursor(5.into(), Some("descending"));
    let s = start(None, Some(&c), Ascending).unwrap();
    assert_eq!((s.order, s.after), (Descending, Some(5)));
    assert_eq!(
        start(Some(Ascending), Some(&c), Ascending)
            .err()
            .unwrap()
            .to_string(),
        "The cursor continues a descending scan; the query asks for ascending"
    );
    // A cursor without an order continues in the fallback order.
    let s = start(None, Some(&cursor(5.into(), None)), Descending).unwrap();
    assert_eq!((s.order, s.after), (Descending, Some(5)));
    for bad in [
        cursor("x".into(), None),
        cursor(5.into(), Some("sideways")),
        cursor(1.5.into(), None),
    ] {
        assert_eq!(
            start(Some(Ascending), Some(&bad), Ascending)
                .err()
                .unwrap()
                .to_string(),
            "Invalid storage cursor"
        );
    }
    let next = next_cursor(7, Descending);
    assert_eq!(
        serde_json::to_value(&next).unwrap(),
        serde_json::json!({ "after": 7, "order": "descending" })
    );
}

// ---- image.ts / image-processor.ts ----

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = (data.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&[0, 0, 0, 0]);
    out
}
fn png(animated: bool) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend(png_chunk(b"IHDR", &[0; 13]));
    if animated {
        bytes.extend(png_chunk(b"acTL", &[0; 8]));
    }
    bytes.extend(png_chunk(b"IDAT", &[0; 4]));
    bytes
}

#[tokio::test]
async fn image_types_are_detected_from_headers_and_streams() {
    let mut webp = b"RIFF".to_vec();
    webp.extend([0, 0, 0, 0]);
    webp.extend(b"WEBP");
    let cases: Vec<(Vec<u8>, Option<&str>)> = vec![
        (vec![0xff, 0xd8, 0xff, 0xe0, 0], Some("image/jpeg")),
        (vec![0xff, 0xd8, 0xff, 0xf7, 0], None),
        (b"GIF87a....".to_vec(), Some("image/gif")),
        (b"GIF89a....".to_vec(), Some("image/gif")),
        (webp, Some("image/webp")),
        (png(false), Some("image/png")),
        (png(true), None),
        (b"plain text".to_vec(), None),
    ];
    let directory = tempfile::tempdir().unwrap();
    let fs = NativeFileSystem::new(directory.path()).unwrap();
    let context = Context::default();
    for (index, (bytes, expected)) in cases.iter().enumerate() {
        assert_eq!(
            detect_supported_image_mime_type(bytes),
            *expected,
            "case {index}"
        );
        let name = format!("image-{index}");
        std::fs::write(directory.path().join(&name), bytes).unwrap();
        let reader = fs
            .open_binary_reader(Path::new(&name), false, &context)
            .await
            .unwrap();
        let size = bytes.len() as u64;
        assert_eq!(
            detect_supported_image_mime_type_of(reader.as_ref(), size, &context)
                .await
                .unwrap(),
            *expected,
            "streamed case {index}"
        );
        reader.close(&context).await.unwrap();
    }
    for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
        assert!(inline_image_type(mime), "{mime}");
    }
    for mime in ["image/bmp", "image/svg+xml", "text/plain", ""] {
        assert!(!inline_image_type(mime), "{mime}");
    }
}

// ---- chord output.ts ----

#[test]
fn character_end_backs_up_to_a_utf8_boundary() {
    let bytes = "aé🙂z".as_bytes();
    assert_eq!(bytes, [97, 195, 169, 240, 159, 153, 130, 122]);
    // Upstream, byte for byte: [0, 1, 1, 3, 3, 3, 3, 7, 8, 9, 10]. An index past the end is returned unchanged.
    let ends: Vec<usize> = (0..=10)
        .map(|i| pi_chord::output::character_end(bytes, i))
        .collect();
    assert_eq!(ends, [0, 1, 1, 3, 3, 3, 3, 7, 8, 9, 10]);
    assert_eq!(pi_chord::output::character_end(&[], 5), 5);
}

// ---- codemode/describe.ts ----

#[test]
fn namespace_of_picks_the_longest_matching_prefix() {
    use pi_durable::codemode::describe::{CodemodeNamespace, namespace_of};
    let namespace = |name: &str, prefix: &str| -> CodemodeNamespace {
        serde_json::from_value(serde_json::json!({ "name": name, "prefix": prefix })).unwrap()
    };
    let all = [
        namespace("a", "pi."),
        namespace("b", "pi.models."),
        namespace("c", "other."),
    ];
    let name = |tool: &str| namespace_of(&js(tool), &all).map(|n| n.name.to_string_lossy());
    assert_eq!(name("pi.read"), Some("a".into()));
    assert_eq!(name("pi.models.classify"), Some("b".into()));
    assert_eq!(name("other.x"), Some("c".into()));
    assert_eq!(name("nope"), None);
}

// ---- env-line-scan.test.ts ----

fn random(seed: u32) -> impl FnMut() -> f64 {
    let mut state = seed;
    move || {
        state = state.wrapping_add(0x6d2b79f5);
        let mut t = state;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        f64::from(t ^ (t >> 14)) / 4294967296.0
    }
}
const PIECES: [&[u8]; 13] = [
    &[0x0a],
    &[0x0a],
    &[0x61],
    &[0x62, 0x63],
    &[0xef, 0xbb, 0xbf],
    &[0xc3, 0xa9],
    &[0xe2, 0x82, 0xac],
    &[0xf0, 0x9f, 0x98, 0x80],
    &[0xe2, 0x82],
    &[0xff],
    &[0x80],
    &[0xf0, 0x9f],
    &[0x0d, 0x0a],
];
fn random_file(next: &mut impl FnMut() -> f64) -> Vec<u8> {
    let mut bytes = Vec::new();
    let pieces = (next() * 60.0).floor() as usize;
    for _ in 0..pieces {
        bytes.extend_from_slice(PIECES[(next() * PIECES.len() as f64).floor() as usize]);
    }
    bytes
}
/// `new TextDecoder("utf-8", { ignoreBOM })`: replacement decoding that drops a leading BOM unless ignored.
fn text_decode(bytes: &[u8], ignore_bom: bool) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    match text.strip_prefix('\u{feff}') {
        Some(rest) if !ignore_bom => rest.to_owned(),
        _ => text,
    }
}

#[test]
fn stream_decoder_keeps_a_bom_that_does_not_start_the_stream() {
    let bytes = [0xe2, 0x82, 0xef, 0xbb, 0xbf, 0x61];
    let mut decoder = StreamDecoder::default();
    let mut text: String = bytes
        .iter()
        .map(|byte| decoder.decode(Some(&[*byte])))
        .collect();
    text.push_str(&decoder.decode(None));
    assert_eq!(text, text_decode(&bytes, false));
    assert_eq!(text, "\u{fffd}\u{feff}a");
    assert!(starts_with_bom(&[0xef, 0xbb, 0xbf, 1]));
    assert!(!starts_with_bom(&[0xef, 0xbb]));
}

#[test]
fn stream_decoder_decodes_any_chunking_like_the_whole_stream() {
    for seed in 1..=5000 {
        let mut next = random(seed);
        let bytes = random_file(&mut next);
        let mut decoder = StreamDecoder::default();
        let mut text = String::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let size = 1 + (next() * 7.0).floor() as usize;
            let end = (offset + size).min(bytes.len());
            text.push_str(&decoder.decode(Some(&bytes[offset..end])));
            offset = end;
        }
        text.push_str(&decoder.decode(None));
        assert_eq!(text, text_decode(&bytes, false), "seed {seed}");
    }
}

#[test]
fn line_scanner_agrees_with_decoding_and_splitting_the_whole_file() {
    for seed in 1..=5000 {
        let mut next = random(seed);
        let file = random_file(&mut next);
        let whole = text_decode(&file, false);
        let lines: Vec<&str> = whole.split('\n').collect();
        let start_line = (next() * (lines.len() + 2) as f64).floor() as usize;
        let end_line = (next() >= 0.3)
            .then(|| start_line + 1 + (next() * (lines.len() + 1) as f64).floor() as usize);
        let mut scanner =
            LineScanner::new(start_line as f64, end_line.map(|end| end as f64)).unwrap();
        let mut offset = 0;
        while offset < file.len() {
            let size = 1 + (next() * 7.0).floor() as usize;
            let end = (offset + size).min(file.len());
            scanner.push(&file[offset..end]);
            offset = end;
        }
        let scan = scanner.finish();
        let message = format!("seed {seed}");
        assert_eq!(scan.newlines, lines.len() as u64 - 1, "{message}");
        let selected = &lines[start_line.min(lines.len())
            ..end_line
                .unwrap_or(usize::MAX)
                .min(lines.len())
                .max(start_line.min(lines.len()))];
        let decode = |from: u64, to: u64| text_decode(&file[from as usize..to as usize], from > 0);
        assert_eq!(
            decode(scan.start, scan.end),
            selected.join("\n"),
            "{message}"
        );
        assert_eq!(
            scan.selected_bytes,
            selected.join("\n").len() as u64,
            "{message}"
        );
        if start_line < lines.len() {
            assert_eq!(
                decode(scan.start, scan.first_line_end),
                lines[start_line],
                "{message}"
            );
            assert_eq!(
                scan.first_line_bytes,
                lines[start_line].len() as u64,
                "{message}"
            );
            let last_line = end_line.unwrap_or(lines.len()).min(lines.len()) - 1;
            let last_end = if last_line + 1 < lines.len() {
                file[scan.last_line_start as usize..]
                    .iter()
                    .position(|b| *b == 0x0a)
                    .map(|p| p + scan.last_line_start as usize)
                    .unwrap()
            } else {
                file.len()
            };
            assert_eq!(
                decode(scan.last_line_start, last_end as u64),
                lines[last_line],
                "{message}"
            );
        } else {
            assert_eq!(
                (scan.start, scan.end, scan.selected_bytes),
                (file.len() as u64, file.len() as u64, 0),
                "{message}"
            );
        }
    }
}

#[test]
fn line_scanner_rejects_empty_or_invalid_ranges() {
    assert!(LineScanner::new(2.0, Some(2.0)).is_err());
    assert!(LineScanner::new(-1.0, None).is_err());
    assert!(LineScanner::new(1.5, None).is_err());
}
