//! Port of `packages/coding-agent/test/tools.test.ts`, `edit-tool-legacy-input.test.ts` and the
//! tool regression tests in `test/suite/regressions` (3302, 3303, 5208, 5303, 6104).

use pi_agent::types::{AgentToolRef, AgentToolResult};
use pi_coding_agent::core::tools::bash::{
    BashOperations, BashOperationsOptions, BashOperationsResult, BashToolOptions, create_bash_tool,
    create_bash_tool_definition, create_local_bash_operations, create_local_shell_operations,
    CreateLocalBashOperationsOptions,
};
use pi_coding_agent::core::tools::edit::{EditOperations, EditToolOptions, create_edit_tool, create_edit_tool_definition};
use pi_coding_agent::core::tools::edit_diff::{ComputeEditsDiffResult, Edit, EditDiffError, compute_edits_diff};
use pi_coding_agent::core::tools::find::{
    FindOperations, FindToolOptions, PathFlavor, create_find_tool, create_find_tool_definition,
    relativize_find_result_path,
};
use pi_coding_agent::core::tools::{create_grep_tool, create_ls_tool, create_read_tool, create_write_tool};
use pi_coding_agent::utils::shell::{ShellConfig, ShellConfigCommandTransport};
use pi_coding_agent::utils::tools_manager::{GetToolPathTool, ensure_tool};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

async fn run(tool: &AgentToolRef, params: Value) -> anyhow::Result<AgentToolResult> {
    tool.execute("test-call", params, CancellationToken::new(), Arc::new(|_| {})).await
}

fn text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .map(|t| t.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn details(result: &AgentToolResult) -> &Value {
    result.details.as_ref().expect("details")
}

fn p(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(unix)]
fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

fn tiny_bmp_1x1_red_24bpp() -> Vec<u8> {
    let mut buffer = vec![0u8; 58];
    buffer[0..2].copy_from_slice(b"BM");
    buffer[2..6].copy_from_slice(&58u32.to_le_bytes());
    buffer[10..14].copy_from_slice(&54u32.to_le_bytes());
    buffer[14..18].copy_from_slice(&40u32.to_le_bytes());
    buffer[18..22].copy_from_slice(&1i32.to_le_bytes());
    buffer[22..26].copy_from_slice(&1i32.to_le_bytes());
    buffer[26..28].copy_from_slice(&1u16.to_le_bytes());
    buffer[28..30].copy_from_slice(&24u16.to_le_bytes());
    buffer[34..38].copy_from_slice(&4u32.to_le_bytes());
    buffer[56] = 0xff;
    buffer
}

/// Apply a unified diff produced by `generate_unified_patch` (jsdiff `applyPatch` for our patches).
fn apply_patch(original: &str, patch: &str) -> String {
    let lines: Vec<&str> = original.split_inclusive('\n').collect();
    let mut out = String::new();
    let mut cursor = 0usize;
    let patch_lines: Vec<&str> = patch.split('\n').collect();
    let mut i = 0;
    while i < patch_lines.len() {
        let line = patch_lines[i];
        if let Some(header) = line.strip_prefix("@@ -") {
            let old_start: usize = header.split([',', ' ']).next().unwrap().parse().unwrap();
            let start = old_start.saturating_sub(1);
            while cursor < start {
                out.push_str(lines[cursor]);
                cursor += 1;
            }
            i += 1;
            while i < patch_lines.len() && !patch_lines[i].starts_with("@@") {
                let l = patch_lines[i];
                let no_newline = patch_lines.get(i + 1) == Some(&"\\ No newline at end of file");
                if let Some(rest) = l.strip_prefix(' ') {
                    out.push_str(rest);
                    if !no_newline {
                        out.push('\n');
                    }
                    cursor += 1;
                } else if l.starts_with('-') {
                    cursor += 1;
                } else if let Some(rest) = l.strip_prefix('+') {
                    out.push_str(rest);
                    if !no_newline {
                        out.push('\n');
                    }
                }
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    while cursor < lines.len() {
        out.push_str(lines[cursor]);
        cursor += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// read tool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_file_contents_within_limits() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("test.txt");
    let content = "Hello, world!\nLine 2\nLine 3";
    fs::write(&file, content).unwrap();
    let tool = create_read_tool(&p(dir.path()), None);
    let result = run(&tool, json!({ "path": p(&file) })).await.unwrap();
    assert_eq!(text(&result), content);
    assert!(result.details.is_none());
    assert_eq!(result.structured_content, Some(json!(content)));
}

#[tokio::test]
async fn read_non_existent_file() {
    let dir = tempfile::tempdir().unwrap();
    let tool = create_read_tool(&p(dir.path()), None);
    let err = run(&tool, json!({ "path": p(&dir.path().join("nonexistent.txt")) }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("ENOENT"), "{err}");
}

#[tokio::test]
async fn read_truncates_by_lines_and_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let tool = create_read_tool(&p(dir.path()), None);

    let file = dir.path().join("large.txt");
    let lines: Vec<String> = (1..=2500).map(|i| format!("Line {i}")).collect();
    fs::write(&file, lines.join("\n")).unwrap();
    let result = run(&tool, json!({ "path": p(&file) })).await.unwrap();
    let output = text(&result);
    assert!(output.contains("Line 1\n"));
    assert!(output.contains("Line 2000"));
    assert!(!output.contains("Line 2001"));
    assert!(output.contains("[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]"));
    let truncation = &details(&result)["truncation"];
    assert_eq!(truncation["truncated"], json!(true));
    assert_eq!(truncation["truncatedBy"], json!("lines"));
    assert_eq!(truncation["totalLines"], json!(2500));
    assert_eq!(truncation["outputLines"], json!(2000));

    let file = dir.path().join("large-bytes.txt");
    let lines: Vec<String> = (1..=500).map(|i| format!("Line {i}: {}", "x".repeat(200))).collect();
    fs::write(&file, lines.join("\n")).unwrap();
    let output = text(&run(&tool, json!({ "path": p(&file) })).await.unwrap());
    assert!(output.contains("Line 1:"));
    let re = regex::Regex::new(r"\[Showing lines 1-\d+ of 500 \(.* limit\)\. Use offset=\d+ to continue\.\]").unwrap();
    assert!(re.is_match(&output), "{output}");
}

#[tokio::test]
async fn read_offset_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let tool = create_read_tool(&p(dir.path()), None);
    let file = dir.path().join("offset.txt");
    let lines: Vec<String> = (1..=100).map(|i| format!("Line {i}")).collect();
    fs::write(&file, lines.join("\n")).unwrap();

    let output = text(&run(&tool, json!({ "path": p(&file), "offset": 51 })).await.unwrap());
    assert!(!output.contains("Line 50\n"));
    assert!(output.contains("Line 51"));
    assert!(output.contains("Line 100"));
    assert!(!output.contains("Use offset="));

    let output = text(&run(&tool, json!({ "path": p(&file), "limit": 10 })).await.unwrap());
    assert!(output.contains("Line 10"));
    assert!(!output.contains("Line 11"));
    assert!(output.contains("[90 more lines in file. Use offset=11 to continue.]"));

    let output = text(&run(&tool, json!({ "path": p(&file), "offset": 41, "limit": 20 })).await.unwrap());
    assert!(!output.contains("Line 40"));
    assert!(output.contains("Line 41"));
    assert!(output.contains("Line 60"));
    assert!(!output.contains("Line 61"));
    assert!(output.contains("[40 more lines in file. Use offset=61 to continue.]"));

    let short = dir.path().join("short.txt");
    fs::write(&short, "Line 1\nLine 2\nLine 3").unwrap();
    let err = run(&tool, json!({ "path": p(&short), "offset": 100 })).await.unwrap_err();
    assert_eq!(err.to_string(), "Offset 100 is beyond end of file (3 lines total)");
}

#[tokio::test]
async fn read_detects_images_by_magic() {
    use base64::Engine;
    let dir = tempfile::tempdir().unwrap();
    let tool = create_read_tool(&p(dir.path()), None);
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg==")
        .unwrap();
    let file = dir.path().join("image.txt");
    fs::write(&file, png).unwrap();
    let result = run(&tool, json!({ "path": p(&file) })).await.unwrap();
    assert!(result.content[0].as_text().is_some());
    assert!(text(&result).contains("Read image file [image/png]"));
    let image = result.content.iter().find_map(|c| c.as_image()).expect("image block");
    assert_eq!(image.mime_type, "image/png");
    assert!(!image.data.is_empty());
    assert_eq!(
        result.structured_content,
        Some(json!({ "type": "image", "data": image.data, "mimeType": "image/png", "note": text(&result) }))
    );

    let bmp = dir.path().join("image.bmp");
    fs::write(&bmp, tiny_bmp_1x1_red_24bpp()).unwrap();
    let result = run(&tool, json!({ "path": p(&bmp) })).await.unwrap();
    assert!(text(&result).contains("Read image file [image/png]"));
    assert!(text(&result).contains("[Image converted from image/bmp to image/png.]"));
    let image = result.content.iter().find_map(|c| c.as_image()).expect("image block");
    assert_eq!(image.mime_type, "image/png");
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(&image.data).unwrap()[0], 0x89);

    let fake = dir.path().join("not-an-image.png");
    fs::write(&fake, "definitely not a png").unwrap();
    let result = run(&tool, json!({ "path": p(&fake) })).await.unwrap();
    assert!(text(&result).contains("definitely not a png"));
    assert!(result.content.iter().all(|c| c.as_image().is_none()));
}

// ---------------------------------------------------------------------------
// write tool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_file_and_parent_directories() {
    let dir = tempfile::tempdir().unwrap();
    let tool = create_write_tool(&p(dir.path()), None);
    let file = dir.path().join("write-test.txt");
    let result = run(&tool, json!({ "path": p(&file), "content": "Test content" })).await.unwrap();
    assert_eq!(text(&result), format!("Successfully wrote to {}", p(&file)));
    assert!(result.details.is_none());
    assert_eq!(fs::read_to_string(&file).unwrap(), "Test content");

    let nested = dir.path().join("nested").join("dir").join("test.txt");
    let result = run(&tool, json!({ "path": p(&nested), "content": "Nested content" })).await.unwrap();
    assert!(text(&result).contains("Successfully wrote"));
    assert_eq!(fs::read_to_string(&nested).unwrap(), "Nested content");
}

// ---------------------------------------------------------------------------
// edit tool
// ---------------------------------------------------------------------------

async fn edit(dir: &Path, file: &Path, edits: Value) -> anyhow::Result<AgentToolResult> {
    let tool = create_edit_tool(&p(dir), None);
    run(&tool, json!({ "path": p(file), "edits": edits })).await
}

#[tokio::test]
async fn edit_replaces_text_with_diff_and_patch() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("edit-test.txt");
    fs::write(&file, "Hello, world!").unwrap();
    let result = edit(dir.path(), &file, json!([{ "oldText": "world", "newText": "testing" }])).await.unwrap();
    assert!(text(&result).contains("Successfully replaced"));
    let d = details(&result);
    assert!(d["diff"].as_str().unwrap().contains("testing"));
    let patch = d["patch"].as_str().unwrap();
    assert!(patch.contains("--- "));
    assert!(patch.contains("+++ "));
    assert!(patch.contains("@@"));
    assert!(patch.contains("-Hello, world!"));
    assert!(patch.contains("+Hello, testing!"));
    assert_eq!(apply_patch("Hello, world!", patch), "Hello, testing!");
}

#[tokio::test]
async fn edit_error_cases() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("edit-test.txt");
    fs::write(&file, "Hello, world!").unwrap();
    let err = edit(dir.path(), &file, json!([{ "oldText": "nonexistent", "newText": "testing" }]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Could not find the exact text"));

    let missing = dir.path().join("missing.txt");
    let err = edit(dir.path(), &missing, json!([{ "oldText": "hello", "newText": "world" }]))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), format!("Could not edit file: {}. Error code: ENOENT.", p(&missing)));

    fs::write(&file, "foo foo foo").unwrap();
    let err = edit(dir.path(), &file, json!([{ "oldText": "foo", "newText": "bar" }])).await.unwrap_err();
    assert!(err.to_string().contains("Found 3 occurrences"));

    fs::write(&file, "hello\nworld\n").unwrap();
    let err = edit(dir.path(), &file, json!([])).await.unwrap_err();
    assert!(err.to_string().contains("edits must contain at least one replacement"));

    fs::write(&file, "one\ntwo\nthree\n").unwrap();
    let err = edit(
        dir.path(),
        &file,
        json!([
            { "oldText": "one\ntwo\n", "newText": "ONE\nTWO\n" },
            { "oldText": "two\nthree\n", "newText": "TWO\nTHREE\n" }
        ]),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("overlap"));

    let original = "alpha\nbeta\ngamma\n";
    fs::write(&file, original).unwrap();
    let err = edit(
        dir.path(),
        &file,
        json!([
            { "oldText": "alpha\n", "newText": "ALPHA\n" },
            { "oldText": "missing\n", "newText": "MISSING\n" }
        ]),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("Could not find"));
    assert_eq!(fs::read_to_string(&file).unwrap(), original);
}

#[cfg(unix)]
#[tokio::test]
async fn edit_reports_eacces() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("edit-readonly.txt");
    fs::write(&file, "hello\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
    let err = edit(dir.path(), &file, json!([{ "oldText": "hello", "newText": "world" }])).await.unwrap_err();
    assert_eq!(err.to_string(), format!("Could not edit file: {}. Error code: EACCES.", p(&file)));

    let unreadable = dir.path().join("unreadable-preview.txt");
    fs::write(&unreadable, "hello\n").unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o222)).unwrap();
    let result = compute_edits_diff(
        &p(&unreadable),
        &[Edit { old_text: "hello".into(), new_text: "world".into() }],
        &p(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        ComputeEditsDiffResult::EditDiffError(EditDiffError {
            error: format!("Could not edit file: {}. Error code: EACCES.", p(&unreadable))
        })
    );
}

#[tokio::test]
async fn edit_reports_unknown_access_errors() {
    let dir = tempfile::tempdir().unwrap();
    let tool = create_edit_tool(
        &p(dir.path()),
        Some(&EditToolOptions {
            operations: Some(EditOperations {
                access: Arc::new(|_| Box::pin(async { anyhow::bail!("disk offline") })),
                read_file: Arc::new(|_| Box::pin(async { Ok(b"hello\n".to_vec()) })),
                write_file: Arc::new(|_, _| Box::pin(async { Ok(()) })),
            }),
        }),
    );
    let err = run(&tool, json!({ "path": "broken.txt", "edits": [{ "oldText": "hello", "newText": "world" }] }))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "Could not edit file: broken.txt. Error: disk offline.");
}

#[tokio::test]
async fn edit_diff_preview_for_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing-preview.txt");
    let result = compute_edits_diff(
        &p(&missing),
        &[Edit { old_text: "hello".into(), new_text: "world".into() }],
        &p(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        ComputeEditsDiffResult::EditDiffError(EditDiffError {
            error: format!("Could not edit file: {}. Error code: ENOENT.", p(&missing))
        })
    );
}

#[tokio::test]
async fn edit_multiple_disjoint_regions() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("edit-multi.txt");
    fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").unwrap();
    let result = edit(
        dir.path(),
        &file,
        json!([{ "oldText": "alpha\n", "newText": "ALPHA\n" }, { "oldText": "gamma\n", "newText": "GAMMA\n" }]),
    )
    .await
    .unwrap();
    assert!(text(&result).contains("Successfully replaced 2 block(s)"));
    assert_eq!(fs::read_to_string(&file).unwrap(), "ALPHA\nbeta\nGAMMA\ndelta\n");
    let diff = details(&result)["diff"].as_str().unwrap();
    assert!(diff.contains("ALPHA") && diff.contains("GAMMA"));

    // Large unchanged gaps collapse.
    let file = dir.path().join("edit-multi-large-gap.txt");
    let lines: Vec<String> = (1..=600).map(|i| format!("line {i:03}")).collect();
    fs::write(&file, format!("{}\n", lines.join("\n"))).unwrap();
    let result = edit(
        dir.path(),
        &file,
        json!([
            { "oldText": "line 100\n", "newText": "LINE 100\n" },
            { "oldText": "line 300\n", "newText": "LINE 300\n" },
            { "oldText": "line 500\n", "newText": "LINE 500\n" }
        ]),
    )
    .await
    .unwrap();
    let diff = details(&result)["diff"].as_str().unwrap();
    assert!(diff.contains("LINE 100") && diff.contains("LINE 300") && diff.contains("LINE 500"));
    assert!(diff.contains("..."));
    assert!(!diff.contains("line 250"));
    assert!(diff.split('\n').count() < 50);

    // Edits match against the original file.
    let file = dir.path().join("edit-multi-original.txt");
    fs::write(&file, "foo\nbar\nbaz\n").unwrap();
    edit(
        dir.path(),
        &file,
        json!([{ "oldText": "foo\n", "newText": "foo bar\n" }, { "oldText": "bar\n", "newText": "BAR\n" }]),
    )
    .await
    .unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "foo bar\nBAR\nbaz\n");
}

#[tokio::test]
async fn edit_fuzzy_matching() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("fuzzy.txt");
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "line one   \nline two  \nline three\n",
            json!([{ "oldText": "line one\nline two\n", "newText": "replaced\n" }]),
            "replaced\nline three\n",
        ),
        (
            "你好，世界\n你好（世界）\n",
            json!([{ "oldText": "你好,世界\n你好(世界)\n", "newText": "你好，pi\n你好(pi)\n" }]),
            "你好，pi\n你好(pi)\n",
        ),
        (
            "ＡＢＣ１２３\ncafe\u{0301}\n",
            json!([{ "oldText": "ABC123\ncafé\n", "newText": "XYZ789\ncoffee\n" }]),
            "XYZ789\ncoffee\n",
        ),
        (
            "console.log(\u{2018}hello\u{2019});\n",
            json!([{ "oldText": "console.log('hello');", "newText": "console.log('world');" }]),
            "console.log('world');\n",
        ),
        (
            "const msg = \u{201C}Hello World\u{201D};\n",
            json!([{ "oldText": "const msg = \"Hello World\";", "newText": "const msg = \"Goodbye\";" }]),
            "const msg = \"Goodbye\";\n",
        ),
        (
            "range: 1\u{2013}5\nbreak\u{2014}here\n",
            json!([{ "oldText": "range: 1-5\nbreak-here", "newText": "range: 10-50\nbreak--here" }]),
            "range: 10-50\nbreak--here\n",
        ),
        (
            "hello\u{00A0}world\n",
            json!([{ "oldText": "hello world", "newText": "hello universe" }]),
            "hello universe\n",
        ),
        (
            "const x = 'exact';\nconst y = 'other';\n",
            json!([{ "oldText": "const x = 'exact';", "newText": "const x = 'changed';" }]),
            "const x = 'changed';\nconst y = 'other';\n",
        ),
        (
            "console.log(\u{2018}hello\u{2019});\nhello\u{00A0}world\n",
            json!([
                { "oldText": "console.log('hello');\n", "newText": "console.log('world');\n" },
                { "oldText": "hello world\n", "newText": "hello universe\n" }
            ]),
            "console.log('world');\nhello universe\n",
        ),
    ];
    for (original, edits, expected) in cases {
        fs::write(&file, original).unwrap();
        let result = edit(dir.path(), &file, edits).await.unwrap();
        assert!(text(&result).contains("Successfully replaced"));
        assert_eq!(fs::read_to_string(&file).unwrap(), expected);
    }

    fs::write(&file, "completely different content\n").unwrap();
    let err = edit(dir.path(), &file, json!([{ "oldText": "this does not exist", "newText": "x" }]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Could not find the exact text"));

    fs::write(&file, "hello world   \nhello world\n").unwrap();
    let err = edit(dir.path(), &file, json!([{ "oldText": "hello world", "newText": "replaced" }]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Found 2 occurrences"));
}

#[tokio::test]
async fn edit_fuzzy_preserves_untouched_lines() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("fuzzy-preserve.txt");

    let original = "replace me   \nafter   \n";
    fs::write(&file, original).unwrap();
    let result = edit(dir.path(), &file, json!([{ "oldText": "replace me\n", "newText": "after\n" }]))
        .await
        .unwrap();
    let expected = "after\nafter   \n";
    assert_eq!(fs::read_to_string(&file).unwrap(), expected);
    assert_eq!(apply_patch(original, details(&result)["patch"].as_str().unwrap()), expected);

    let original = "keep before  \nfirst target  \nfirst after\nkeep middle   \nsecond target  \nsecond after\nkeep after  \n";
    fs::write(&file, original).unwrap();
    let result = edit(
        dir.path(),
        &file,
        json!([
            { "oldText": "first target\nfirst after", "newText": "FIRST\nFIRST2" },
            { "oldText": "second target\nsecond after", "newText": "SECOND\nSECOND2" }
        ]),
    )
    .await
    .unwrap();
    let expected = "keep before  \nFIRST\nFIRST2\nkeep middle   \nSECOND\nSECOND2\nkeep after  \n";
    assert_eq!(fs::read_to_string(&file).unwrap(), expected);
    assert_eq!(apply_patch(original, details(&result)["patch"].as_str().unwrap()), expected);
}

#[tokio::test]
async fn edit_crlf_and_bom_handling() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("crlf.txt");
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "line one\r\nline two\r\nline three\r\n",
            json!([{ "oldText": "line two\n", "newText": "replaced line\n" }]),
            "line one\r\nreplaced line\r\nline three\r\n",
        ),
        (
            "first\r\nsecond\r\nthird\r\n",
            json!([{ "oldText": "second\n", "newText": "REPLACED\n" }]),
            "first\r\nREPLACED\r\nthird\r\n",
        ),
        (
            "first\nsecond\nthird\n",
            json!([{ "oldText": "second\n", "newText": "REPLACED\n" }]),
            "first\nREPLACED\nthird\n",
        ),
        (
            "\u{FEFF}first\r\nsecond\r\nthird\r\n",
            json!([{ "oldText": "second\n", "newText": "REPLACED\n" }]),
            "\u{FEFF}first\r\nREPLACED\r\nthird\r\n",
        ),
        (
            "\u{FEFF}first\r\nsecond\r\nthird\r\nfourth\r\n",
            json!([{ "oldText": "second\n", "newText": "SECOND\n" }, { "oldText": "fourth\n", "newText": "FOURTH\n" }]),
            "\u{FEFF}first\r\nSECOND\r\nthird\r\nFOURTH\r\n",
        ),
    ];
    for (original, edits, expected) in cases {
        fs::write(&file, original).unwrap();
        let result = edit(dir.path(), &file, edits).await.unwrap();
        assert!(text(&result).contains("Successfully replaced"));
        assert_eq!(fs::read_to_string(&file).unwrap(), expected);
    }

    fs::write(&file, "hello\r\nworld\r\n---\r\nhello\nworld\n").unwrap();
    let err = edit(dir.path(), &file, json!([{ "oldText": "hello\nworld\n", "newText": "replaced\n" }]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Found 2 occurrences"));
}

// edit-tool-legacy-input.test.ts
#[tokio::test]
async fn edit_prepare_arguments_compat() {
    let definition = create_edit_tool_definition(".", None);
    let props = definition.parameters["properties"].as_object().unwrap();
    assert!(!props.contains_key("oldText") && !props.contains_key("newText"));
    let prepare = definition.prepare_arguments.clone().unwrap();

    assert_eq!(
        prepare(json!({ "path": "file.txt", "oldText": "before", "newText": "after" })),
        json!({ "path": "file.txt", "edits": [{ "oldText": "before", "newText": "after" }] })
    );
    assert_eq!(
        prepare(json!({ "path": "file.txt", "edits": [{ "oldText": "a", "newText": "b" }], "oldText": "c", "newText": "d" })),
        json!({ "path": "file.txt", "edits": [{ "oldText": "a", "newText": "b" }, { "oldText": "c", "newText": "d" }] })
    );
    let valid = json!({ "path": "file.txt", "edits": [{ "oldText": "a", "newText": "b" }] });
    assert_eq!(prepare(valid.clone()), valid);
    assert_eq!(prepare(Value::Null), Value::Null);
    assert_eq!(prepare(json!("garbage")), json!("garbage"));
    assert_eq!(
        prepare(json!({ "path": "file.txt", "edits": "[{\"oldText\":\"a\",\"newText\":\"b\"}]" })),
        json!({ "path": "file.txt", "edits": [{ "oldText": "a", "newText": "b" }] })
    );
    assert_eq!(
        prepare(json!({ "path": "file.txt", "edits": "not json" })),
        json!({ "path": "file.txt", "edits": "not json" })
    );
    assert_eq!(
        prepare(json!({ "path": "file.txt", "edits": { "oldText": "a", "newText": "b" } })),
        json!({ "path": "file.txt", "edits": [{ "oldText": "a", "newText": "b" }] })
    );

    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("legacy.txt"), "before\n").unwrap();
    let definition = create_edit_tool_definition(&p(dir.path()), None);
    let prepared = prepare(json!({ "path": "legacy.txt", "oldText": "before", "newText": "after" }));
    let result = (definition.execute)("tool-1".into(), prepared, None, None, None).await.unwrap();
    assert_eq!(text(&result), "Successfully replaced 1 block(s) in legacy.txt.");
    assert_eq!(fs::read_to_string(dir.path().join("legacy.txt")).unwrap(), "after\n");
}

// ---------------------------------------------------------------------------
// bash tool
// ---------------------------------------------------------------------------

fn bash_with_ops(dir: &Path, operations: BashOperations) -> AgentToolRef {
    create_bash_tool(
        &p(dir),
        Some(&BashToolOptions {
            operations: Some(operations),
            ..Default::default()
        }),
    )
}

fn ops<F, Fut>(f: F) -> BashOperations
where
    F: Fn(BashOperationsOptions) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = anyhow::Result<BashOperationsResult>> + Send + 'static,
{
    let f = Arc::new(f);
    BashOperations {
        exec: Arc::new(move |_command, _cwd, options| Box::pin(f(options))),
    }
}

#[tokio::test]
async fn bash_executes_commands_and_reports_exit_codes() {
    let dir = tempfile::tempdir().unwrap();
    let bash = create_bash_tool(&p(dir.path()), None);
    let result = run(&bash, json!({ "command": "echo 'test output'" })).await.unwrap();
    assert!(text(&result).contains("test output"));
    assert!(result.details.is_none());

    let result = run(&bash, json!({ "command": "echo out; exit 3" })).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(text(&result), "out\n\n\nCommand exited with code 3");
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["output"], json!("out\n"));
    assert_eq!(structured["truncated"], json!(false));
    assert_eq!(structured["exit_code"], json!(3));
    assert!(structured["wall_time_seconds"].is_number());
    assert!(structured.get("full_output_path").is_none());

    let ok = run(&bash, json!({ "command": "echo fine" })).await.unwrap();
    assert!(ok.is_error.is_none());
    assert_eq!(ok.structured_content.as_ref().unwrap()["output"], json!("fine\n"));
    assert_eq!(ok.structured_content.as_ref().unwrap()["exit_code"], json!(0));

    let empty = run(&bash, json!({ "command": "true" })).await.unwrap();
    assert_eq!(text(&empty), "(no output)");
    assert_eq!(empty.structured_content.as_ref().unwrap()["output"], json!(""));
}

#[tokio::test]
async fn bash_structured_output_limits() {
    let dir = tempfile::tempdir().unwrap();
    let bash = create_bash_tool(&p(dir.path()), None);
    let medium = run(&bash, json!({ "command": "seq 1 3000" })).await.unwrap();
    assert!(!text(&medium).contains("\n1\n2\n"));
    assert_eq!(details(&medium)["truncation"]["truncated"], json!(true));
    let structured = medium.structured_content.as_ref().unwrap();
    assert_eq!(structured["truncated"], json!(false));
    let expected: String = (1..=3000).map(|i| format!("{i}\n")).collect();
    assert_eq!(structured["output"], json!(expected));

    let large = run(&bash, json!({ "command": "seq 1 300000" })).await.unwrap();
    let structured = large.structured_content.as_ref().unwrap();
    assert_eq!(structured["truncated"], json!(true));
    let output = structured["output"].as_str().unwrap();
    assert!(output.starts_with("1\n2\n3\n"));
    assert!(output.ends_with("299999\n300000\n"));
    assert!(regex::Regex::new(r"\n\n\[\.\.\. \d+ bytes omitted \.\.\.\]\n\n").unwrap().is_match(output));
    assert!(output.len() < 1024 * 1024 + 100);
    let path = structured["full_output_path"].as_str().unwrap();
    assert_eq!(Some(path), details(&large)["fullOutputPath"].as_str());
    assert!(fs::read_to_string(path).unwrap().ends_with("300000\n"));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_maps_signal_kills_to_128_plus_signal() {
    let dir = tempfile::tempdir().unwrap();
    let operations = create_local_bash_operations(None);
    for (signal, exit_code) in [("KILL", 137), ("TERM", 143)] {
        let result = (operations.exec)(
            format!("kill -{signal} $$"),
            p(dir.path()),
            BashOperationsOptions {
                on_data: Arc::new(|_| {}),
                signal: None,
                timeout: None,
                env: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, Some(exit_code));
    }

    let bash = create_bash_tool(&p(dir.path()), None);
    for (signal, exit_code) in [("KILL", 137), ("TERM", 143)] {
        let result = run(&bash, json!({ "command": format!("printf 'before-kill\\n'; kill -{signal} $$") }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        let re = regex::Regex::new(&format!(r"before-kill\s+Command exited with code {exit_code}$")).unwrap();
        assert!(re.is_match(&text(&result)), "{}", text(&result));
    }
}

#[tokio::test]
async fn bash_rejects_null_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let bash = bash_with_ops(
        dir.path(),
        ops(|options| async move {
            (options.on_data)(b"partial\n".to_vec());
            Ok(BashOperationsResult { exit_code: None })
        }),
    );
    let err = run(&bash, json!({ "command": "remote" })).await.unwrap_err();
    assert!(regex::Regex::new(r"partial\s+Command terminated without an exit code$").unwrap().is_match(&err.to_string()));
}

#[tokio::test]
async fn bash_respects_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let bash = create_bash_tool(&p(dir.path()), None);
    let started = std::time::Instant::now();
    let err = run(&bash, json!({ "command": "sleep 10", "timeout": 0.05 })).await.unwrap_err();
    assert!(err.to_string().to_lowercase().contains("timed out"), "{err}");
    assert!(err.to_string().contains("Command timed out after 0.05 seconds"));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));

    let err = run(&bash, json!({ "command": "echo hi", "timeout": -1 })).await.unwrap_err();
    assert!(err.to_string().contains("Invalid timeout: must be a finite number of seconds"));
}

#[tokio::test]
async fn bash_aborts_running_commands() {
    let dir = tempfile::tempdir().unwrap();
    let bash = create_bash_tool(&p(dir.path()), None);
    let signal = CancellationToken::new();
    let cancel = signal.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let started = std::time::Instant::now();
    let err = bash
        .execute("abort", json!({ "command": "echo started; sleep 10" }), signal, Arc::new(|_| {}))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "started\n\n\nCommand aborted");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[tokio::test]
async fn bash_includes_full_output_path_for_timeout_and_abort() {
    let dir = tempfile::tempdir().unwrap();
    let showing_lines = regex::Regex::new(r"\[Showing lines \d+-\d+ of \d+\. Full output: ").unwrap();
    let full_output = regex::Regex::new(r"Full output: ([^\]\n]+)").unwrap();
    for (error, expected) in [("timeout:5", "Command timed out after 5 seconds"), ("aborted", "Command aborted")] {
        let bash = bash_with_ops(
            dir.path(),
            ops(move |options| async move {
                for i in 1..=3000 {
                    (options.on_data)(format!("{i}\n").into_bytes());
                }
                anyhow::bail!(error)
            }),
        );
        let message = run(&bash, json!({ "command": "chatty-fail" })).await.unwrap_err().to_string();
        assert!(message.contains(expected));
        assert!(showing_lines.is_match(&message));
        assert!(!message.contains("Full output: undefined"));
        let path = full_output.captures(&message).unwrap()[1].to_string();
        let full = fs::read_to_string(&path).unwrap();
        assert!(full.contains("1\n2\n3"));
        assert!(full.contains("2998\n2999\n3000"));
    }
}

#[tokio::test]
async fn bash_missing_cwd_and_spawn_errors() {
    let bash = create_bash_tool("/this/directory/definitely/does/not/exist/12345", None);
    let err = run(&bash, json!({ "command": "echo test" })).await.unwrap_err();
    assert!(err.to_string().contains("Working directory does not exist"));

    let dir = tempfile::tempdir().unwrap();
    let operations = create_local_shell_operations(
        "bash",
        Arc::new(|| {
            Ok(ShellConfig {
                shell: "/nonexistent-shell-path-xyz123".into(),
                args: vec!["-c".into()],
                command_transport: None,
            })
        }),
    );
    let bash = bash_with_ops(dir.path(), operations);
    let err = run(&bash, json!({ "command": "echo test" })).await.unwrap_err();
    assert!(err.to_string().contains("ENOENT"), "{err}");

    let operations = create_local_bash_operations(Some(&CreateLocalBashOperationsOptions {
        shell_path: Some("/custom/bash".into()),
    }));
    let err = (operations.exec)(
        "echo test".into(),
        p(dir.path()),
        BashOperationsOptions { on_data: Arc::new(|_| {}), signal: None, timeout: None, env: None },
    )
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), "Custom shell path not found: /custom/bash");
}

#[cfg(unix)]
#[tokio::test]
async fn bash_sends_commands_over_stdin_when_required() {
    let dir = tempfile::tempdir().unwrap();
    let operations = create_local_shell_operations(
        "bash",
        Arc::new(|| {
            Ok(ShellConfig {
                shell: "cat".into(),
                args: vec![],
                command_transport: Some(ShellConfigCommandTransport::Stdin),
            })
        }),
    );
    let chunks = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
    let sink = chunks.clone();
    let command = "name='World'; echo \"Hello, ${name}!\"";
    let result = (operations.exec)(
        command.into(),
        p(dir.path()),
        BashOperationsOptions {
            on_data: Arc::new(move |data| sink.lock().extend(data)),
            signal: None,
            timeout: None,
            env: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(String::from_utf8(chunks.lock().clone()).unwrap(), command);
}

#[tokio::test]
async fn bash_command_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let with_prefix = |prefix: Option<&str>| {
        create_bash_tool(
            &p(dir.path()),
            Some(&BashToolOptions {
                command_prefix: prefix.map(str::to_string),
                ..Default::default()
            }),
        )
    };
    let result = run(&with_prefix(Some("export TEST_VAR=hello")), json!({ "command": "echo $TEST_VAR" })).await.unwrap();
    assert_eq!(text(&result).trim(), "hello");
    let result = run(&with_prefix(Some("echo prefix-output")), json!({ "command": "echo command-output" })).await.unwrap();
    assert_eq!(text(&result).trim(), "prefix-output\ncommand-output");
    let result = run(&with_prefix(None), json!({ "command": "echo no-prefix" })).await.unwrap();
    assert_eq!(text(&result).trim(), "no-prefix");
}

#[tokio::test]
async fn bash_coalesces_streaming_updates() {
    let dir = tempfile::tempdir().unwrap();
    let bash = bash_with_ops(
        dir.path(),
        ops(|options| async move {
            for i in 0..5000 {
                (options.on_data)(format!("line {i}\n").into_bytes());
            }
            Ok(BashOperationsResult { exit_code: Some(0) })
        }),
    );
    let updates = Arc::new(parking_lot::Mutex::new(0usize));
    let counter = updates.clone();
    let result = bash
        .execute(
            "chatty",
            json!({ "command": "chatty" }),
            CancellationToken::new(),
            Arc::new(move |_| *counter.lock() += 1),
        )
        .await
        .unwrap();
    assert!(*updates.lock() < 25);
    assert!(text(&result).contains("line 4999"));
}

#[tokio::test]
async fn bash_trailing_newline_line_count_and_split_utf8() {
    let dir = tempfile::tempdir().unwrap();
    let bash = bash_with_ops(
        dir.path(),
        ops(|options| async move {
            for i in 1..=4000 {
                (options.on_data)(format!("line-{i:04}\n").into_bytes());
            }
            Ok(BashOperationsResult { exit_code: Some(0) })
        }),
    );
    let result = run(&bash, json!({ "command": "many-lines" })).await.unwrap();
    let output = text(&result);
    assert_eq!(details(&result)["truncation"]["totalLines"], json!(4000));
    assert_eq!(details(&result)["truncation"]["outputLines"], json!(2000));
    assert!(output.contains("line-2001") && output.contains("line-4000"));
    assert!(regex::Regex::new(r"\[Showing lines 2001-4000 of 4000\. Full output: ").unwrap().is_match(&output));
    assert!(!output.contains("4001"));

    let bash = bash_with_ops(
        dir.path(),
        ops(|options| async move {
            let euro = "€\n".as_bytes();
            (options.on_data)(euro[..1].to_vec());
            (options.on_data)(euro[1..].to_vec());
            Ok(BashOperationsResult { exit_code: Some(0) })
        }),
    );
    let result = run(&bash, json!({ "command": "split-utf8" })).await.unwrap();
    assert_eq!(text(&result).trim(), "€");
}

#[tokio::test]
async fn bash_local_operations_with_env_and_line_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let operations = create_local_bash_operations(None);
    let chunks = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
    let sink = chunks.clone();
    let mut env = pi_coding_agent::utils::shell::get_shell_env();
    env.insert("TEST_LOCAL_BASH_OPS".into(), "from-local-ops".into());
    let result = (operations.exec)(
        "echo $TEST_LOCAL_BASH_OPS".into(),
        p(dir.path()),
        BashOperationsOptions {
            on_data: Arc::new(move |data| sink.lock().extend(data)),
            signal: None,
            timeout: None,
            env: Some(env),
        },
    )
    .await
    .unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(String::from_utf8(chunks.lock().clone()).unwrap().trim(), "from-local-ops");

    let bash = create_bash_tool(&p(dir.path()), None);
    let result = run(&bash, json!({ "command": "seq 3000" })).await.unwrap();
    let output = text(&result);
    assert_eq!(details(&result)["truncation"]["truncatedBy"], json!("lines"));
    let path = details(&result)["fullOutputPath"].as_str().unwrap().to_string();
    assert!(regex::Regex::new(r"\[Showing lines \d+-\d+ of \d+\. Full output: ").unwrap().is_match(&output));
    let full = fs::read_to_string(path).unwrap();
    assert!(full.contains("1\n2\n3") && full.contains("2998\n2999\n3000"));
}

// 5208-late-bash-output.test.ts: output callbacks after the operations resolve are ignored.
#[tokio::test]
async fn bash_ignores_output_after_operations_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let bash = bash_with_ops(
        dir.path(),
        ops(|options| async move {
            (options.on_data)(b"before\n".to_vec());
            let on_data = options.on_data.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                on_data(b"late\n".to_vec());
            });
            Ok(BashOperationsResult { exit_code: Some(0) })
        }),
    );
    let result = run(&bash, json!({ "command": "late-output" })).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(text(&result).trim(), "before");
}

// 5303-bash-output-truncation.test.ts: output written after the shell exits by a descendant
// that keeps writing is kept.
#[cfg(unix)]
#[tokio::test]
async fn bash_keeps_output_from_writing_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let bash = create_bash_tool(&p(dir.path()), None);
    let result = run(
        &bash,
        json!({ "command": "echo HEAD; (for i in 1 2 3 4 5 6; do sleep 0.05; echo TICK$i; done) & exit 0" }),
    )
    .await
    .unwrap();
    let output = text(&result);
    assert!(output.contains("HEAD"));
    assert!(output.contains("TICK6"), "{output}");
}

#[tokio::test]
async fn bash_definition_prompt_metadata() {
    let definition = create_bash_tool_definition(".", None);
    assert_eq!(definition.prompt_snippet.as_deref(), Some("Execute bash commands (ls, grep, find, etc.)"));
    assert_eq!(
        definition.prompt_guidelines,
        Some(vec!["You can inspect PI_* environment variables for current model and session details.".to_string()])
    );
    let without_env = create_bash_tool_definition(
        ".",
        Some(&BashToolOptions {
            expose_session_environment: Some(false),
            ..Default::default()
        }),
    );
    assert!(without_env.prompt_guidelines.is_none());
}

// ---------------------------------------------------------------------------
// grep tool
// ---------------------------------------------------------------------------

async fn rg_available() -> bool {
    ensure_tool(GetToolPathTool::Rg, None).await.ok().flatten().is_some()
}

#[tokio::test]
async fn grep_single_file_and_context() {
    if !rg_available().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let grep = create_grep_tool(&p(dir.path()), None);
    let file = dir.path().join("example.txt");
    fs::write(&file, "first line\nmatch line\nlast line").unwrap();
    let output = text(&run(&grep, json!({ "pattern": "match", "path": p(&file) })).await.unwrap());
    assert!(output.contains("example.txt:2: match line"), "{output}");

    let file = dir.path().join("context.txt");
    fs::write(&file, "before\nmatch one\nafter\nmiddle\nmatch two\nafter two").unwrap();
    let output = text(
        &run(&grep, json!({ "pattern": "match", "path": p(&file), "limit": 1, "context": 1 }))
            .await
            .unwrap(),
    );
    assert!(output.contains("context.txt-1- before"));
    assert!(output.contains("context.txt:2: match one"));
    assert!(output.contains("context.txt-3- after"));
    assert!(output.contains("[1 matches limit reached. Use limit=2 for more, or refine pattern]"));
    assert!(!output.contains("match two"));
}

#[tokio::test]
async fn grep_ignores_ripgrep_config_path() {
    const CHILD: &str = "PI_GREP_CONFIG_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("rg-config");
        fs::write(&config, "--fixed-strings\n").unwrap();
        // Only the isolated test process inherits this config; concurrent tests
        // never observe a process-wide environment mutation.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "grep_ignores_ripgrep_config_path", "--nocapture"])
            .env(CHILD, "1")
            .env("RIPGREP_CONFIG_PATH", &config)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let rg = ensure_tool(GetToolPathTool::Rg, None)
        .await
        .unwrap()
        .expect("ripgrep required for regression");
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("fixture.txt");
    fs::write(&file, "aXb\na.b\n").unwrap();
    // Confirm the inherited config changes a plain rg search to literal mode.
    let configured = std::process::Command::new(rg)
        .args(["--", "a.b"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(configured.status.success());
    assert_eq!(String::from_utf8(configured.stdout).unwrap(), "a.b\n");

    let grep = create_grep_tool(&p(dir.path()), None);
    let result = run(&grep, json!({ "pattern": "a.b", "path": p(&file), "literal": false }))
        .await
        .unwrap();
    assert_eq!(text(&result), "fixture.txt:1: aXb\nfixture.txt:2: a.b");
    let result = run(&grep, json!({ "pattern": "a.b", "path": p(&file), "literal": true }))
        .await
        .unwrap();
    assert_eq!(text(&result), "fixture.txt:2: a.b");
}

#[cfg(unix)]
#[tokio::test]
async fn grep_treats_flag_like_patterns_as_text() {
    use std::os::unix::fs::PermissionsExt;
    if !rg_available().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("grep-injection-marker");
    let payload = dir.path().join("payload.sh");
    fs::write(&payload, format!("#!/bin/sh\necho executed > {}\ncat \"$1\"\n", p(&marker))).unwrap();
    fs::set_permissions(&payload, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(dir.path().join("target.txt"), "target\n").unwrap();
    let grep = create_grep_tool(&p(dir.path()), None);
    let result = run(&grep, json!({ "pattern": format!("--pre={}", p(&payload)), "path": p(dir.path()) }))
        .await
        .unwrap();
    assert!(text(&result).contains("No matches found"));
    assert!(!marker.exists());
}

#[tokio::test]
async fn grep_relative_paths_in_directories() {
    if !rg_available().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/a.txt"), "needle\n").unwrap();
    let grep = create_grep_tool(&p(dir.path()), None);
    let output = text(&run(&grep, json!({ "pattern": "needle" })).await.unwrap());
    assert_eq!(output, "src/a.txt:1: needle");
}

// ---------------------------------------------------------------------------
// find tool
// ---------------------------------------------------------------------------

async fn fd_available() -> bool {
    ensure_tool(GetToolPathTool::Fd, None).await.ok().flatten().is_some()
}

#[tokio::test]
async fn find_hidden_files_gitignore_and_errors() {
    if !fd_available().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let find = create_find_tool(&p(dir.path()), None);
    fs::create_dir(dir.path().join(".secret")).unwrap();
    fs::write(dir.path().join(".secret/hidden.txt"), "hidden").unwrap();
    fs::write(dir.path().join("visible.txt"), "visible").unwrap();
    let output = text(&run(&find, json!({ "pattern": "**/*.txt", "path": p(dir.path()) })).await.unwrap());
    let lines: Vec<&str> = output.split('\n').map(str::trim).filter(|l| !l.is_empty()).collect();
    assert!(lines.contains(&"visible.txt"));
    assert!(lines.contains(&".secret/hidden.txt"));

    fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(dir.path().join("ignored.txt"), "ignored").unwrap();
    let output = text(&run(&find, json!({ "pattern": "**/*.txt", "path": p(dir.path()) })).await.unwrap());
    assert!(output.contains("visible.txt"));
    assert!(!output.contains("ignored.txt"));

    let err = run(&find, json!({ "pattern": "[", "path": p(dir.path()) })).await.unwrap_err();
    assert!(
        regex::Regex::new(r"(?i)error parsing glob|fd exited with code 1|fd error").unwrap().is_match(&err.to_string()),
        "{err}"
    );

    let output = text(&run(&find, json!({ "pattern": "--help", "path": p(dir.path()) })).await.unwrap());
    assert!(output.contains("No files found matching pattern"));
}

// 3302-find-path-glob.test.ts
#[tokio::test]
async fn find_path_containing_globs() {
    if !fd_available().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src/nested")).unwrap();
    fs::write(dir.path().join("src/nested/a.spec.ts"), "").unwrap();
    fs::write(dir.path().join("b.spec.ts"), "").unwrap();
    let find = create_find_tool(&p(dir.path()), None);
    let output = text(&run(&find, json!({ "pattern": "src/**/*.spec.ts" })).await.unwrap());
    assert_eq!(output, "src/nested/a.spec.ts");
}

// 3303-find-nested-gitignore.test.ts
#[tokio::test]
async fn find_scopes_nested_gitignore_files() {
    if !fd_available().await {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    fs::create_dir_all(r.join("a/deep")).unwrap();
    fs::create_dir_all(r.join("b")).unwrap();
    fs::write(r.join("a/.gitignore"), "ignored.txt\n").unwrap();
    fs::write(r.join("a/deep/.gitignore"), "secret.txt\n").unwrap();
    for file in ["a/ignored.txt", "a/kept.txt", "a/deep/ignored.txt", "a/deep/secret.txt", "a/deep/kept.txt", "b/ignored.txt", "b/kept.txt", "root.txt"] {
        fs::write(r.join(file), "").unwrap();
    }
    let definition = create_find_tool_definition(&p(r), None);
    let result = (definition.execute)("call-1".into(), json!({ "pattern": "**/*.txt" }), None, None, None)
        .await
        .unwrap();
    let mut files: Vec<String> = text(&result)
        .split('\n')
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('['))
        .collect();
    files.sort();
    assert_eq!(files, vec!["a/deep/kept.txt", "a/kept.txt", "b/ignored.txt", "b/kept.txt", "root.txt"]);
}

// 6104-find-root-relativization.test.ts
#[test]
fn find_relativizes_root_search_paths() {
    let win = Some(PathFlavor::Win32);
    let posix = Some(PathFlavor::Posix);
    assert_eq!(relativize_find_result_path("I:\\AI\\Models\\TextGen\\gemma4\\", "I:\\", win), "AI/Models/TextGen/gemma4/");
    assert_eq!(relativize_find_result_path("I:/AI/Models/TextGen/gemma4/", "I:\\", win), "AI/Models/TextGen/gemma4/");
    assert_eq!(relativize_find_result_path("I:\\AI\\Models\\", "I:\\AI", win), "Models/");
    assert_eq!(
        relativize_find_result_path("I:\\AI\\Models2\\file.txt", "I:\\AI\\Models", win),
        "../Models2/file.txt"
    );
    assert_eq!(relativize_find_result_path("AI\\Models\\TextGen\\gemma4\\", "I:\\", win), "AI/Models/TextGen/gemma4/");
    assert_eq!(relativize_find_result_path("/home/user/file.txt", "/", posix), "home/user/file.txt");
    assert_eq!(relativize_find_result_path("/home/user/project/", "/", posix), "home/user/project/");
    assert_eq!(relativize_find_result_path("/home/user/file\\", "/home/user", posix), "file\\");
    assert_eq!(
        relativize_find_result_path("/tmp/results/file.txt", "/workspace/project", posix),
        "../../tmp/results/file.txt"
    );
    assert_eq!(
        relativize_find_result_path("/tmp/results/dir/", "/workspace/project", posix),
        "../../tmp/results/dir/"
    );
}

#[tokio::test]
async fn find_custom_glob_relativizes_against_root() {
    let definition = create_find_tool_definition(
        "/",
        Some(&FindToolOptions {
            operations: Some(FindOperations {
                exists: Arc::new(|_| Box::pin(async { Ok(true) })),
                glob: Arc::new(|_, _, _| {
                    Box::pin(async {
                        Ok(vec!["/home/user/project/".to_string(), "/home/user/project/file.txt".to_string()])
                    })
                }),
            }),
        }),
    );
    let result = (definition.execute)("call-1".into(), json!({ "pattern": "**" }), None, None, None)
        .await
        .unwrap();
    assert_eq!(text(&result), "home/user/project/\nhome/user/project/file.txt");
}

// ---------------------------------------------------------------------------
// ls tool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ls_lists_dotfiles_and_directories() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".hidden-file"), "secret").unwrap();
    fs::create_dir(dir.path().join(".hidden-dir")).unwrap();
    fs::write(dir.path().join("B.txt"), "").unwrap();
    fs::write(dir.path().join("a.txt"), "").unwrap();
    let ls = create_ls_tool(&p(dir.path()), None);
    let output = text(&run(&ls, json!({ "path": p(dir.path()) })).await.unwrap());
    assert_eq!(output, ".hidden-dir/\n.hidden-file\na.txt\nB.txt");

    let output = text(&run(&ls, json!({ "limit": 2 })).await.unwrap());
    assert_eq!(output, ".hidden-dir/\n.hidden-file\n\n[2 entries limit reached. Use limit=4 for more]");

    let empty = tempfile::tempdir().unwrap();
    let ls = create_ls_tool(&p(empty.path()), None);
    assert_eq!(text(&run(&ls, json!({})).await.unwrap()), "(empty directory)");
    let err = run(&ls, json!({ "path": "missing" })).await.unwrap_err();
    assert_eq!(err.to_string(), format!("Path not found: {}", p(&empty.path().join("missing"))));
}

// ---------------------------------------------------------------------------
// tools.test.ts "tool cwd resolution": tools resolve paths against `ctx.cwd` when a tool context is
// passed. TS fakes the context with `{ cwd }`; here a minimal `ExtensionContextHost` provides it.
// ---------------------------------------------------------------------------

mod ctx_cwd {
    use super::*;
    use pi_coding_agent::core::extensions::runner::{NoOpUIContext, stale_extension_context};
    use pi_coding_agent::core::extensions::types::{
        ExtensionContext, ExtensionContextActions, ExtensionContextHost, ExtensionMode, ExtensionToolContext,
        ExtensionUIContext, ToolDefinition,
    };
    use pi_coding_agent::core::model_registry::ModelRegistry;
    use pi_coding_agent::core::session_manager::ReadonlySessionManager;
    use pi_coding_agent::core::tools::bash::{BashToolOptions, create_bash_tool_definition};
    use pi_coding_agent::core::tools::find::create_find_tool_definition;
    use pi_coding_agent::core::tools::grep::create_grep_tool_definition;
    use pi_coding_agent::core::tools::ls::create_ls_tool_definition;
    use pi_coding_agent::core::tools::read::create_read_tool_definition;
    use pi_coding_agent::core::tools::write::create_write_tool_definition;

    struct CwdHost(String, Option<pi_ai::types::Model>);

    /// The stale context's default actions, plus a current model.
    struct ModelActions {
        model: Option<pi_ai::types::Model>,
        inner: Arc<dyn ExtensionContextActions>,
    }

    impl ExtensionContextActions for ModelActions {
        fn get_model(&self) -> Option<pi_ai::types::Model> {
            self.model.clone()
        }
        fn get_scoped_models(&self) -> Vec<pi_coding_agent::core::model_resolver::ScopedModel> {
            self.inner.get_scoped_models()
        }
        fn is_idle(&self) -> bool {
            self.inner.is_idle()
        }
        fn is_project_trusted(&self) -> bool {
            self.inner.is_project_trusted()
        }
        fn get_signal(&self) -> Option<CancellationToken> {
            self.inner.get_signal()
        }
        fn abort(&self) {
            self.inner.abort()
        }
        fn has_pending_messages(&self) -> bool {
            self.inner.has_pending_messages()
        }
        fn shutdown(&self) {
            self.inner.shutdown()
        }
        fn get_context_usage(&self) -> Option<pi_coding_agent::core::extensions::types::ContextUsage> {
            self.inner.get_context_usage()
        }
        fn compact(&self, options: pi_coding_agent::core::extensions::types::CompactOptions) {
            self.inner.compact(options)
        }
        fn get_system_prompt(&self) -> String {
            self.inner.get_system_prompt()
        }
    }

    impl ExtensionContextHost for CwdHost {
        fn assert_active(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn ui(&self) -> Arc<dyn ExtensionUIContext> {
            Arc::new(NoOpUIContext)
        }
        fn mode(&self) -> ExtensionMode {
            ExtensionMode::Print
        }
        fn has_ui(&self) -> bool {
            false
        }
        fn cwd(&self) -> String {
            self.0.clone()
        }
        fn session_manager(&self) -> Option<ReadonlySessionManager> {
            None
        }
        fn model_registry(&self) -> Option<Arc<ModelRegistry>> {
            None
        }
        fn thinking_level(&self) -> Option<pi_agent::types::ThinkingLevel> {
            None
        }
        fn actions(&self) -> Arc<dyn ExtensionContextActions> {
            Arc::new(ModelActions { model: self.1.clone(), inner: stale_extension_context(None).host().actions() })
        }
    }

    fn fake_ctx(cwd: &Path) -> ExtensionToolContext {
        fake_ctx_with_model(cwd, None)
    }

    pub(super) fn fake_ctx_with_model(cwd: &Path, model: Option<pi_ai::types::Model>) -> ExtensionToolContext {
        ExtensionToolContext {
            ctx: ExtensionContext::new(Arc::new(CwdHost(cwd.to_string_lossy().into_owned(), model))),
            tool_call_id: "test".into(),
            signal: None,
        }
    }

    pub(super) async fn execute(tool: &ToolDefinition, id: &str, args: Value, cwd: &Path) -> AgentToolResult {
        (tool.execute)(id.to_string(), args, None, None, Some(fake_ctx(cwd))).await.unwrap()
    }

    #[tokio::test]
    async fn read_uses_ctx_cwd_when_provided() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ctx-cwd-read.txt"), "hello from ctx.cwd").unwrap();
        let tool = create_read_tool_definition("/", None);
        let result = execute(&tool, "test-read-ctx-cwd", json!({ "path": "ctx-cwd-read.txt" }), dir.path()).await;
        assert!(text(&result).contains("hello from ctx.cwd"));
    }

    #[tokio::test]
    async fn write_uses_ctx_cwd_when_provided() {
        let dir = tempfile::tempdir().unwrap();
        let tool = create_write_tool_definition("/", None);
        execute(
            &tool,
            "test-write-ctx-cwd",
            json!({ "path": "ctx-cwd-write.txt", "content": "written via ctx.cwd" }),
            dir.path(),
        )
        .await;
        assert_eq!(fs::read_to_string(dir.path().join("ctx-cwd-write.txt")).unwrap(), "written via ctx.cwd");
    }

    #[tokio::test]
    async fn edit_uses_ctx_cwd_when_provided() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ctx-cwd-edit.txt"), "old text").unwrap();
        let tool = create_edit_tool_definition("/", None);
        execute(
            &tool,
            "test-edit-ctx-cwd",
            json!({ "path": "ctx-cwd-edit.txt", "edits": [{ "oldText": "old text", "newText": "new text" }] }),
            dir.path(),
        )
        .await;
        assert_eq!(fs::read_to_string(dir.path().join("ctx-cwd-edit.txt")).unwrap(), "new text");
    }

    #[tokio::test]
    async fn grep_uses_ctx_cwd_when_provided() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ctx-cwd-grep.txt"), "match in ctx.cwd").unwrap();
        let tool = create_grep_tool_definition("/", None);
        let result = execute(&tool, "test-grep-ctx-cwd", json!({ "pattern": "match" }), dir.path()).await;
        assert!(text(&result).contains("ctx-cwd-grep.txt"), "{}", text(&result));
    }

    #[tokio::test]
    async fn find_uses_ctx_cwd_when_provided() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ctx-cwd-find.txt"), "find me").unwrap();
        let tool = create_find_tool_definition("/", None);
        let result = execute(&tool, "test-find-ctx-cwd", json!({ "pattern": "ctx-cwd-find.txt" }), dir.path()).await;
        assert!(text(&result).contains("ctx-cwd-find.txt"), "{}", text(&result));
    }

    #[tokio::test]
    async fn ls_uses_ctx_cwd_when_provided() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ctx-cwd-ls.txt"), "list me").unwrap();
        let tool = create_ls_tool_definition("/", None);
        let result = execute(&tool, "test-ls-ctx-cwd", json!({}), dir.path()).await;
        assert!(text(&result).contains("ctx-cwd-ls.txt"), "{}", text(&result));
    }

    #[tokio::test]
    async fn bash_uses_ctx_cwd_when_provided() {
        let dir = tempfile::tempdir().unwrap();
        let tool = create_bash_tool_definition(
            "/",
            Some(&BashToolOptions { expose_session_environment: Some(false), ..Default::default() }),
        );
        let result = execute(&tool, "test-bash-ctx-cwd", json!({ "command": "pwd" }), dir.path()).await;
        assert!(text(&result).contains(&dir.path().to_string_lossy().into_owned()), "{}", text(&result));
    }
}

// ---------------------------------------------------------------------------
// block-images.test.ts and image-resize-callers.test.ts. The image-resize-callers cases where
// `resizeImage` is mocked to fail ("text-only output when auto-resize cannot produce a safe
// image") are not ported: the resizer is not injectable. The model resize profile case checks the
// resized dimensions instead of the mock's arguments.
// ---------------------------------------------------------------------------

mod images {
    use super::*;
    use pi_coding_agent::cli::file_processor::{ProcessFileOptions, process_file_arguments};
    use pi_coding_agent::core::settings_manager::SettingsManager;
    use pi_coding_agent::core::tools::read::create_read_tool_definition;

    const TINY_PNG_BASE64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";

    fn tiny_png() -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(TINY_PNG_BASE64).unwrap()
    }

    fn tiny_bmp_1x1_red_24bpp() -> Vec<u8> {
        let mut buffer = vec![0u8; 58];
        buffer[0..2].copy_from_slice(b"BM");
        buffer[2..6].copy_from_slice(&58u32.to_le_bytes());
        buffer[10..14].copy_from_slice(&54u32.to_le_bytes());
        buffer[14..18].copy_from_slice(&40u32.to_le_bytes());
        buffer[18..22].copy_from_slice(&1i32.to_le_bytes());
        buffer[22..26].copy_from_slice(&1i32.to_le_bytes());
        buffer[26..28].copy_from_slice(&1u16.to_le_bytes());
        buffer[28..30].copy_from_slice(&24u16.to_le_bytes());
        buffer[34..38].copy_from_slice(&4u32.to_le_bytes());
        buffer[56] = 0xff;
        buffer
    }

    fn settings(value: Value) -> Arc<SettingsManager> {
        SettingsManager::in_memory_json(value.as_object().unwrap().clone(), None)
    }

    #[test]
    fn block_images_defaults_to_false_and_round_trips() {
        assert!(!settings(json!({})).get_block_images());
        assert!(settings(json!({ "images": { "blockImages": true } })).get_block_images());
        let manager = settings(json!({}));
        manager.set_block_images(true);
        assert!(manager.get_block_images());
        manager.set_block_images(false);
        assert!(!manager.get_block_images());
        let both = settings(json!({ "images": { "autoResize": true, "blockImages": true } }));
        assert!(both.get_image_auto_resize());
        assert!(both.get_block_images());
    }

    #[tokio::test]
    async fn read_tool_always_reads_images_and_text() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("test.png"), tiny_png()).unwrap();
        fs::write(dir.path().join("test.txt"), "Hello, world!").unwrap();
        let tool = create_read_tool(&dir.path().to_string_lossy(), None);
        let image = run(&tool, json!({ "path": dir.path().join("test.png") })).await.unwrap();
        assert!(image.content.iter().any(|c| c.as_text().is_none()));
        let text = run(&tool, json!({ "path": dir.path().join("test.txt") })).await.unwrap();
        assert_eq!(text.content.len(), 1);
        assert!(text.content[0].as_text().unwrap().text.contains("Hello, world!"));
    }

    #[tokio::test]
    async fn file_processor_always_processes_images_converts_bmp_and_reads_text() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("test.png");
        let bmp = dir.path().join("test.bmp");
        let txt = dir.path().join("test.txt");
        fs::write(&png, tiny_png()).unwrap();
        fs::write(&bmp, tiny_bmp_1x1_red_24bpp()).unwrap();
        fs::write(&txt, "Hello, world!").unwrap();

        let result = process_file_arguments(&[png.to_string_lossy().into_owned()], None).await.unwrap();
        assert_eq!(result.images.len(), 1);

        let result = process_file_arguments(&[bmp.to_string_lossy().into_owned()], None).await.unwrap();
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].mime_type, "image/png");
        assert!(result.text.contains("[Image converted from image/bmp to image/png.]"), "{}", result.text);

        let result = process_file_arguments(&[txt.to_string_lossy().into_owned()], None).await.unwrap();
        assert!(result.images.is_empty());
        assert!(result.text.contains("Hello, world!"));
    }

    #[tokio::test]
    async fn file_processor_can_defer_resizing_until_prompt_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("test.png");
        fs::write(&png, tiny_png()).unwrap();
        let result = process_file_arguments(
            &[png.to_string_lossy().into_owned()],
            Some(&ProcessFileOptions { auto_resize_images: Some(false) }),
        )
        .await
        .unwrap();
        assert_eq!(result.images.len(), 1);
        // Not resized or re-encoded.
        assert_eq!(result.images[0].data, TINY_PNG_BASE64);
    }

    #[tokio::test]
    async fn passes_the_current_model_resize_profile_to_the_read_tool() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.png");
        image::RgbImage::from_pixel(2000, 100, image::Rgb([200, 30, 30])).save(&path).unwrap();
        let model: pi_ai::types::Model = serde_json::from_value(json!({
            "id": "vision-model", "name": "Vision model", "api": "test", "provider": "test",
            "baseUrl": "https://example.com", "reasoning": false, "input": ["text", "image"],
            "inputLimits": { "images": { "resize": { "maxWidth": 1234, "maxHeight": 1000, "maxBytes": 500000, "jpegQuality": 70 } } },
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }, "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap();
        let tool = create_read_tool_definition(&dir.path().to_string_lossy(), None);
        let result = (tool.execute)(
            "test-read-model-profile".into(),
            json!({ "path": path }),
            None,
            None,
            Some(super::ctx_cwd::fake_ctx_with_model(dir.path(), Some(model))),
        )
        .await
        .unwrap();
        let image = result.content.iter().find_map(|c| match c {
            pi_ai::types::UserContentBlock::Image(image) => Some(image.clone()),
            _ => None,
        });
        let image = image.expect("image attachment");
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(&image.data).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(decoded.width(), 1234, "resized to the model's maxWidth");
    }
}
