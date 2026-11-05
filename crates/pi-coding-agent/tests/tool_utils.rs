//! Ports of `path-utils.test.ts`, `image-process.test.ts`, `image-processing.test.ts`,
//! `tool-result-images.test.ts`, `tools-manager.test.ts` (offline status),
//! `read-line-range-display.test.ts` (formatting), `tool-system-prompt-contributions.test.ts` and
//! `builtin-tool-strict-mode.test.ts` (tool definitions).

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use pi_ai::types::{ConstrainedSamplingConfig, JsonSchemaStrictness, UserContentBlock};
use pi_coding_agent::core::tools::bash::{BashToolOptions, create_bash_tool_definition};
use pi_coding_agent::core::tools::path_utils::{expand_path, node_path_resolve, resolve_read_path, resolve_to_cwd};
use pi_coding_agent::core::tools::powershell::{PowerShellToolOptions, create_power_shell_tool_definition};
use pi_coding_agent::core::tools::render_utils::format_read_line_range_ref;
use pi_coding_agent::core::tools::{ToolName, create_all_tool_definitions, create_all_tools};
use pi_coding_agent::utils::image_convert::{convert_to_png, load_png_transcoder};
use pi_coding_agent::utils::image_process::{ProcessImageOptions, ProcessImageResult, process_image};
use pi_coding_agent::utils::image_resize::{ImageResizeOptions, ResizedImage, format_dimension_note, resize_image};
use pi_coding_agent::utils::mime::detect_supported_image_mime_type;
use pi_coding_agent::utils::tool_result_images::{NormalizeToolResultImagesOptions, normalize_tool_result_images};
use pi_coding_agent::utils::tools_manager::{GetToolPathTool, ToolStatus, ToolStatusType, ensure_tool};
use serde_json::{Value, json};
use std::fs;
use std::sync::Arc;

fn p(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// path-utils.test.ts
// ---------------------------------------------------------------------------

#[test]
fn expand_path_cases() {
    assert!(!expand_path("~").contains('~'));
    assert!(!expand_path("~/Documents/file.txt").contains("~/"));
    assert_eq!(expand_path("~draft.md"), "~draft.md");
    assert_eq!(expand_path("@~draft.md"), "~draft.md");
    assert_eq!(expand_path("file\u{00A0}name.txt"), "file name.txt");
}

#[test]
fn resolve_to_cwd_cases() {
    let tmp = std::env::temp_dir();
    let absolute = node_path_resolve(&[&p(&tmp), "absolute", "path", "file.txt"]);
    assert_eq!(resolve_to_cwd(&absolute, &node_path_resolve(&[&p(&tmp), "some", "cwd"])), absolute);
    assert_eq!(resolve_to_cwd("relative/file.txt", "/some/cwd"), "/some/cwd/relative/file.txt");
    let cwd = tmp.join("pi-path-utils-cwd");
    assert_eq!(resolve_to_cwd("~draft.md", &p(&cwd)), p(&cwd.join("~draft.md")));
    assert_eq!(resolve_to_cwd("@~draft.md", &p(&cwd)), p(&cwd.join("~draft.md")));
}

#[test]
fn resolve_read_path_variants() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    fs::write(d.join("test-file.txt"), "content").unwrap();
    assert_eq!(resolve_read_path("test-file.txt", &p(d)), p(&d.join("test-file.txt")));

    // NFD file name, NFC input.
    fs::write(d.join("file\u{0065}\u{0301}.txt"), "content").unwrap();
    let result = resolve_read_path("file\u{00e9}.txt", &p(d));
    assert!(result.contains(&p(d)));
    assert_eq!(result, p(&d.join("file\u{0065}\u{0301}.txt")));

    // Curly quote file name, straight quote input.
    fs::write(d.join("Capture d\u{2019}cran.txt"), "content").unwrap();
    assert_eq!(resolve_read_path("Capture d'cran.txt", &p(d)), p(&d.join("Capture d\u{2019}cran.txt")));

    fs::write(d.join("Capture d\u{2019}\u{00e9}cran.txt"), "content").unwrap();
    assert_eq!(
        resolve_read_path("Capture d'\u{00e9}cran.txt", &p(d)),
        p(&d.join("Capture d\u{2019}\u{00e9}cran.txt"))
    );

    // macOS screenshot AM/PM with narrow no-break space (case-insensitive).
    fs::write(d.join("Screenshot 2024-01-01 at 10.00.00\u{202F}AM.png"), "content").unwrap();
    assert_eq!(
        resolve_read_path("Screenshot 2024-01-01 at 10.00.00 AM.png", &p(d)),
        p(&d.join("Screenshot 2024-01-01 at 10.00.00\u{202F}AM.png"))
    );
    fs::write(d.join("Screenshot 2024-01-01 at 11.00.00\u{202F}am.png"), "content").unwrap();
    assert_eq!(
        resolve_read_path("Screenshot 2024-01-01 at 11.00.00 am.png", &p(d)),
        p(&d.join("Screenshot 2024-01-01 at 11.00.00\u{202F}am.png"))
    );
}

// ---------------------------------------------------------------------------
// image tests
// ---------------------------------------------------------------------------

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

fn assert_png_magic(base64_data: &str) {
    let bytes = B64.decode(base64_data).unwrap();
    assert_eq!(&bytes[..4], &[0x89, 0x50, 0x4e, 0x47]);
}

fn png_dimensions(base64_data: &str) -> (u32, u32) {
    let bytes = B64.decode(base64_data).unwrap();
    (
        u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
        u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
    )
}

/// An 8-bit grayscale PNG of arbitrary dimensions (rows shaded by index).
fn create_png(width: u32, height: u32) -> Vec<u8> {
    let image = image::GrayImage::from_fn(width, height, |_, y| image::Luma([(y % 256) as u8]));
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

const TINY_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACAQMAAABIeJ9nAAAAIGNIUk0AAHomAACAhAAA+gAAAIDoAAB1MAAA6mAAADqYAAAXcJy6UTwAAAAGUExURf8AAP///0EdNBEAAAABYktHRAH/Ai3eAAAAB3RJTUUH6gEOADM5Ddoh/wAAAAxJREFUCNdjYGBgAAAABAABJzQnCgAAACV0RVh0ZGF0ZTpjcmVhdGUAMjAyNi0wMS0xNFQwMDo1MTo1NyswMDowMOnKzHgAAAAldEVYdGRhdGU6bW9kaWZ5ADIwMjYtMDEtMTRUMDA6NTE6NTcrMDA6MDCYl3TEAAAAKHRFWHRkYXRlOnRpbWVzdGFtcAAyMDI2LTAxLTE0VDAwOjUxOjU3KzAwOjAwz4JVGwAAAABJRU5ErkJggg==";
const TINY_JPEG: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsNDhIQDQ4RDgsLEBYQERMUFRUVDA8XGBYUGBIUFRT/2wBDAQMEBAUEBQkFBQkUDQsNFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBT/wAARCAACAAIDAREAAhEBAxEB/8QAFAABAAAAAAAAAAAAAAAAAAAACf/EABQQAQAAAAAAAAAAAAAAAAAAAAD/xAAVAQEBAAAAAAAAAAAAAAAAAAAGCf/EABQRAQAAAAAAAAAAAAAAAAAAAAD/2gAMAwEAAhEDEQA/AD3VTB3/2Q==";

/// A 2x1 JPEG with an XMP APP1 segment before an EXIF APP1 segment carrying orientation 6.
fn jpeg_with_xmp_before_orientation() -> String {
    let jpeg = {
        let image = image::RgbImage::from_pixel(2, 1, image::Rgb([200, 10, 10]));
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
        out.into_inner()
    };
    let app1 = |payload: &[u8]| {
        let mut segment = vec![0xff, 0xe1];
        segment.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        segment.extend_from_slice(payload);
        segment
    };
    let xmp = app1(b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>");
    let mut exif = b"Exif\0\0".to_vec();
    exif.extend_from_slice(&[
        0x49, 0x49, 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, 0x01, 0x00, 0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ]);
    let orientation6 = app1(&exif);
    let mut out = jpeg[..2].to_vec();
    out.extend(xmp);
    out.extend(orientation6);
    out.extend_from_slice(&jpeg[2..]);
    B64.encode(out)
}

#[test]
fn mime_detection() {
    assert_eq!(detect_supported_image_mime_type(b"GIF87a").as_deref(), Some("image/gif"));
    assert_eq!(detect_supported_image_mime_type(b"GIF89a").as_deref(), Some("image/gif"));
    assert_eq!(detect_supported_image_mime_type(&tiny_bmp_1x1_red_24bpp()).as_deref(), Some("image/bmp"));
    assert_eq!(detect_supported_image_mime_type(&B64.decode(TINY_PNG).unwrap()).as_deref(), Some("image/png"));
    assert_eq!(detect_supported_image_mime_type(&B64.decode(TINY_JPEG).unwrap()).as_deref(), Some("image/jpeg"));
    assert_eq!(detect_supported_image_mime_type(b"not an image"), None);
}

#[tokio::test]
async fn process_image_converts_bmp() {
    for auto_resize in [Some(false), None] {
        let result = process_image(
            &tiny_bmp_1x1_red_24bpp(),
            "image/bmp",
            Some(&ProcessImageOptions {
                auto_resize_images: auto_resize,
                resize_options: None,
            }),
        )
        .await
        .unwrap();
        let ProcessImageResult::ProcessImageResultVariant1(ok) = result else {
            panic!("expected success");
        };
        assert_eq!(ok.mime_type, "image/png");
        assert!(ok.hints.contains(&"[Image converted from image/bmp to image/png.]".to_string()));
        assert_png_magic(&ok.data);
    }
}

#[tokio::test]
async fn convert_to_png_cases() {
    let result = convert_to_png(TINY_PNG, "image/png").await.unwrap().unwrap();
    assert_eq!(result.data, TINY_PNG);
    assert_eq!(result.mime_type, "image/png");

    let result = convert_to_png(TINY_JPEG, "image/jpeg").await.unwrap().unwrap();
    assert_eq!(result.mime_type, "image/png");
    assert_png_magic(&result.data);

    // EXIF orientation after an XMP APP1 segment: 2x1 rotated to 1x2.
    let result = convert_to_png(&jpeg_with_xmp_before_orientation(), "image/jpeg").await.unwrap().unwrap();
    assert_eq!(png_dimensions(&result.data), (1, 2));

    let transcoder = load_png_transcoder().await.unwrap().unwrap();
    let png = transcoder(&jpeg_with_xmp_before_orientation(), "image/jpeg").unwrap();
    assert_eq!(png_dimensions(&png), (1, 2));
    assert!(transcoder(&B64.encode("not an image"), "image/jpeg").is_none());
}

#[tokio::test]
async fn resize_image_cases() {
    let opts = |max: i64, bytes: i64| ImageResizeOptions {
        max_width: Some(max),
        max_height: Some(max),
        max_bytes: Some(bytes),
        jpeg_quality: None,
    };
    let input = B64.decode(TINY_PNG).unwrap();
    let result = resize_image(&input, "image/png", Some(&opts(100, 1024 * 1024))).await.unwrap().unwrap();
    assert!(!result.was_resized);
    assert_eq!(result.data, TINY_PNG);
    assert_eq!((result.original_width, result.original_height, result.width, result.height), (2, 2, 2, 2));

    let medium = create_png(100, 100);
    let result = resize_image(&medium, "image/png", Some(&opts(50, 1024 * 1024))).await.unwrap().unwrap();
    assert!(result.was_resized);
    assert_eq!((result.original_width, result.original_height), (100, 100));
    assert!(result.width <= 50 && result.height <= 50);

    // Noisy image so PNG does not compress well.
    let noisy = image::RgbImage::from_fn(200, 200, |x, y| {
        let v = (x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)) as u8;
        image::Rgb([v, v.wrapping_mul(3), v.wrapping_mul(7)])
    });
    let mut cursor = std::io::Cursor::new(Vec::new());
    noisy.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
    let large = cursor.into_inner();
    let large_b64_len = B64.encode(&large).len() as i64;
    let result = resize_image(&large, "image/png", Some(&opts(2000, large_b64_len * 9 / 10))).await.unwrap().unwrap();
    assert!((result.data.len() as i64) < large_b64_len);

    assert!(resize_image(&large, "image/png", Some(&opts(2000, 1))).await.unwrap().is_none());

    let result = resize_image(&B64.decode(TINY_JPEG).unwrap(), "image/jpeg", Some(&opts(100, 1024 * 1024)))
        .await
        .unwrap()
        .unwrap();
    assert!(!result.was_resized);
    assert_eq!((result.original_width, result.original_height), (2, 2));
}

#[test]
fn dimension_notes() {
    let mut resized = ResizedImage {
        data: String::new(),
        mime_type: "image/png".into(),
        original_width: 100,
        original_height: 100,
        width: 100,
        height: 100,
        was_resized: false,
    };
    assert!(format_dimension_note(&resized).is_none());
    resized.original_width = 2000;
    resized.original_height = 1000;
    resized.width = 1000;
    resized.height = 500;
    resized.was_resized = true;
    assert_eq!(
        format_dimension_note(&resized).unwrap(),
        "[Image: original 2000x1000, displayed at 1000x500. Multiply coordinates by 2.00 to map to original image.]"
    );
}

fn image_block(bytes: &[u8], mime_type: &str) -> UserContentBlock {
    UserContentBlock::Image(pi_ai::types::ImageContent {
        data: B64.encode(bytes),
        mime_type: mime_type.into(),
    })
}

#[tokio::test]
async fn normalize_tool_result_images_cases() {
    let content = vec![UserContentBlock::text("no images here")];
    assert_eq!(normalize_tool_result_images(&content, None).await.unwrap(), content);

    let content = vec![
        UserContentBlock::text("screenshot"),
        UserContentBlock::Image(pi_ai::types::ImageContent {
            data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==".into(),
            mime_type: "image/png".into(),
        }),
    ];
    assert_eq!(normalize_tool_result_images(&content, None).await.unwrap(), content);

    let big = create_png(2400, 4800);
    let content = vec![image_block(&big, "image/png")];
    let normalized = normalize_tool_result_images(&content, None).await.unwrap();
    assert_eq!(normalized.len(), 2);
    let image = normalized[0].as_image().unwrap();
    let (w, h) = png_dimensions(&image.data);
    assert!(w <= 2000 && h <= 2000);
    assert!(normalized[1].as_text().unwrap().text.contains("original 2400x4800"));

    let no_resize = NormalizeToolResultImagesOptions {
        auto_resize_images: Some(false),
        resize_options: None,
    };
    assert_eq!(normalize_tool_result_images(&content, Some(&no_resize)).await.unwrap(), content);

    let content = vec![image_block(&tiny_bmp_1x1_red_24bpp(), "image/bmp")];
    let normalized = normalize_tool_result_images(&content, Some(&no_resize)).await.unwrap();
    assert_eq!(normalized[0].as_image().unwrap().mime_type, "image/png");
    assert_eq!(normalized[1].as_text().unwrap().text, "[Image converted from image/bmp to image/png.]");

    let content = vec![UserContentBlock::Image(pi_ai::types::ImageContent {
        data: "bm90LWFuLWltYWdl".into(),
        mime_type: "image/png".into(),
    })];
    assert_eq!(normalize_tool_result_images(&content, None).await.unwrap(), content);

    let content = vec![
        UserContentBlock::text("before"),
        image_block(&create_png(2400, 100), "image/png"),
        UserContentBlock::text("after"),
    ];
    let normalized = normalize_tool_result_images(&content, None).await.unwrap();
    let kinds: Vec<&str> = normalized
        .iter()
        .map(|b| if b.as_image().is_some() { "image" } else { "text" })
        .collect();
    assert_eq!(kinds, vec!["text", "image", "text", "text"]);
    assert_eq!(normalized[0], UserContentBlock::text("before"));
    assert_eq!(normalized[3], UserContentBlock::text("after"));
}

// ---------------------------------------------------------------------------
// tools-manager.test.ts (offline mode)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ensure_tool_reports_offline_status() {
    if pi_coding_agent::utils::tools_manager::get_tool_path(GetToolPathTool::Fd).is_some() {
        return;
    }
    // SAFETY: this test binary only reads PI_OFFLINE from this test.
    unsafe { std::env::set_var("PI_OFFLINE", "1") };
    let statuses = Arc::new(parking_lot::Mutex::new(Vec::<ToolStatus>::new()));
    let sink = statuses.clone();
    let result = ensure_tool(GetToolPathTool::Fd, Some(Arc::new(move |status| sink.lock().push(status))))
        .await
        .unwrap();
    unsafe { std::env::remove_var("PI_OFFLINE") };
    assert!(result.is_none());
    let statuses = statuses.lock();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].r#type, ToolStatusType::Warning);
    assert_eq!(statuses[0].message, "fd not found. Offline mode enabled, skipping download.");
}

// ---------------------------------------------------------------------------
// read-line-range-display.test.ts
// ---------------------------------------------------------------------------

#[test]
fn read_line_range_display_cases() {
    let cases: Vec<(Option<Value>, Option<Value>, &str)> = vec![
        (Some(json!(25)), Some(json!(13)), ":25-37"),
        (Some(json!("25")), Some(json!("13")), ":25-37"),
        (Some(Value::Null), Some(json!("13")), ":1-13"),
        (Some(json!("25")), Some(Value::Null), ":25"),
        (None, None, ""),
        (Some(json!("abc")), Some(json!(13)), ""),
        (Some(json!(25)), Some(json!("")), ""),
        (Some(json!(25)), Some(json!("Infinity")), ""),
    ];
    for (offset, limit, expected) in cases {
        assert_eq!(format_read_line_range_ref(offset.as_ref(), limit.as_ref()), expected, "{offset:?} {limit:?}");
    }
}

// ---------------------------------------------------------------------------
// tool-system-prompt-contributions.test.ts and builtin-tool-strict-mode.test.ts
// ---------------------------------------------------------------------------

#[test]
fn tool_definitions_match_prompt_contributions() {
    use pi_coding_agent::core::tools::{bash, edit, find, grep, ls, powershell, read, write};
    let definitions = create_all_tool_definitions("/workspace", None);
    let contributions: Vec<(ToolName, &Value)> = vec![
        (ToolName::Read, &read::READ_TOOL_SYSTEM_PROMPT_CONTRIBUTION),
        (ToolName::Bash, &bash::BASH_TOOL_SYSTEM_PROMPT_CONTRIBUTION),
        (ToolName::Powershell, &powershell::POWERSHELL_TOOL_SYSTEM_PROMPT_CONTRIBUTION),
        (ToolName::Edit, &edit::EDIT_TOOL_SYSTEM_PROMPT_CONTRIBUTION),
        (ToolName::Write, &write::WRITE_TOOL_SYSTEM_PROMPT_CONTRIBUTION),
        (ToolName::Grep, &grep::GREP_TOOL_SYSTEM_PROMPT_CONTRIBUTION),
        (ToolName::Find, &find::FIND_TOOL_SYSTEM_PROMPT_CONTRIBUTION),
        (ToolName::Ls, &ls::LS_TOOL_SYSTEM_PROMPT_CONTRIBUTION),
    ];
    for (name, contribution) in contributions {
        let definition = &definitions[&name];
        assert_eq!(definition.prompt_snippet.as_deref(), contribution["snippet"].as_str(), "{name:?}");
        let guidelines = definition.prompt_guidelines.clone().unwrap_or_default();
        assert_eq!(json!(guidelines), contribution["guidelines"], "{name:?}");
    }
    let bash = create_bash_tool_definition(
        "/workspace",
        Some(&BashToolOptions {
            expose_session_environment: Some(false),
            ..Default::default()
        }),
    );
    assert!(bash.prompt_guidelines.is_none());
    let powershell = create_power_shell_tool_definition(
        "/workspace",
        Some(&PowerShellToolOptions {
            expose_session_environment: Some(false),
            ..Default::default()
        }),
    );
    assert!(powershell.prompt_guidelines.is_none());
}

#[test]
fn strict_sampling_for_builtin_tools() {
    let definitions = create_all_tool_definitions(".", None);
    let tools = create_all_tools(".", None);
    let prefer = ConstrainedSamplingConfig::JsonSchema {
        strict: JsonSchemaStrictness::Prefer,
    };
    for name in [ToolName::Read, ToolName::Bash, ToolName::Powershell, ToolName::Edit, ToolName::Write] {
        assert_eq!(definitions[&name].constrained_sampling.as_ref(), Some(&prefer));
        assert_eq!(tools[&name].constrained_sampling().and_then(|c| c.config()), Some(&prefer));
    }
    for name in [ToolName::Grep, ToolName::Find, ToolName::Ls] {
        assert!(definitions[&name].constrained_sampling.is_none());
    }
    assert_eq!(definitions[&ToolName::Read].parameters["required"], json!(["path"]));
    let names: Vec<&str> = tools.values().map(|t| t.name()).collect();
    assert_eq!(names, vec!["read", "bash", "powershell", "edit", "write", "grep", "find", "ls"]);
}

// ---------------------------------------------------------------------------
// truncate-to-width.test.ts (pi-tui `truncateToWidth` with wide and multi-byte characters)
// ---------------------------------------------------------------------------

mod truncate_to_width {
    use pi_tui::utils::{truncate_to_width, visible_width};

    fn truncate(text: &str, width: usize) -> String {
        truncate_to_width(text, width, "...", false)
    }

    #[test]
    fn truncates_messages_with_unicode_characters_correctly() {
        let max = 67 - 2;
        assert!(visible_width(&truncate("✔ script to run › dev $ concurrently \"vite\" \"node --import tsx ./", max)) <= max);
    }

    #[test]
    fn handles_emoji_characters() {
        let max = 40 - 2;
        assert!(visible_width(&truncate("🎉 Celebration! 🚀 Launch 📦 Package ready for deployment now", max)) <= max);
    }

    #[test]
    fn handles_mixed_ascii_and_wide_characters() {
        let max = 30 - 2;
        assert!(visible_width(&truncate("Hello 世界 Test 你好 More text here that is long", max)) <= max);
    }

    #[test]
    fn does_not_truncate_messages_that_fit() {
        let truncated = truncate("Short message", 48);
        assert_eq!(truncated, "Short message");
        assert!(visible_width(&truncated) <= 48);
    }

    #[test]
    fn adds_ellipsis_when_truncating() {
        let truncated = truncate("This is a very long message that needs to be truncated", 28);
        assert!(truncated.contains("..."));
        assert!(visible_width(&truncated) <= 28);
    }

    #[test]
    fn handles_the_exact_crash_case_from_issue_report() {
        let (terminal_width, cursor_width) = (67, 2);
        let truncated = truncate(
            "✔ script to run › dev $ concurrently \"vite\" \"node --import tsx ./server.ts\"",
            terminal_width - cursor_width,
        );
        assert!(visible_width(&truncated) + cursor_width <= terminal_width);
    }
}

// ---------------------------------------------------------------------------
// max-thinking.test.ts
// ---------------------------------------------------------------------------

mod max_thinking {
    use pi_agent::types::ThinkingLevel;
    use pi_coding_agent::cli::args::is_valid_thinking_level;
    use pi_coding_agent::core::settings_manager::SettingsManager;
    use pi_coding_agent::modes::interactive::theme::theme::load_theme_from_path;

    #[tokio::test]
    async fn is_accepted_by_cli_and_settings() {
        assert!(is_valid_thinking_level("max"));
        let settings = SettingsManager::in_memory(None, None);
        settings.set_default_thinking_level(ThinkingLevel::Max);
        settings.flush().await.unwrap();
        assert_eq!(settings.get_default_thinking_level(), Some(ThinkingLevel::Max));
    }

    #[test]
    fn falls_back_to_thinking_xhigh_for_legacy_themes() {
        let dir = tempfile::tempdir().unwrap();
        let mut dark: serde_json::Value =
            serde_json::from_str(include_str!("../src/modes/interactive/theme/dark.json")).unwrap();
        dark["name"] = serde_json::json!("legacy-theme");
        dark["colors"].as_object_mut().unwrap().remove("thinkingMax");
        let path = dir.path().join("legacy-theme.json");
        std::fs::write(&path, dark.to_string()).unwrap();
        let theme = load_theme_from_path(&path.to_string_lossy(), None).unwrap();
        assert_eq!(
            theme.get_thinking_border_color(ThinkingLevel::Max)("border".to_string()),
            theme.get_thinking_border_color(ThinkingLevel::Xhigh)("border".to_string())
        );
    }
}

// TS builds the built-in tool schemas with TypeBox, so `validateToolArguments` runs
// `Value.Convert` before the JSON Schema coercion: a single edit object becomes a one-element
// array, "" is 0, "1.9" truncates for integers, "TRUE"/"1" are booleans.
#[test]
fn builtin_tool_arguments_are_converted_like_typebox_value_convert() {
    let tools = create_all_tools(".", None);
    for tool in tools.values() {
        assert!(tool.to_tool().typebox, "{}", tool.name());
    }
    let validate = |name: ToolName, arguments: Value| {
        let tool = tools[&name].to_tool();
        let call = pi_ai::types::ToolCall {
            id: "call".into(),
            name: tool.name.clone(),
            arguments: arguments.as_object().unwrap().clone(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        };
        pi_ai::validate_tool_arguments(&tool, &call).map_err(|e| e.0)
    };
    assert_eq!(
        validate(ToolName::Edit, json!({ "path": "f", "edits": { "oldText": "a", "newText": "b" } })).unwrap(),
        json!({ "path": "f", "edits": [{ "oldText": "a", "newText": "b" }] })
    );
    assert_eq!(validate(ToolName::Read, json!({ "path": "f", "limit": "" })).unwrap(), json!({ "path": "f", "limit": 0 }));
    assert_eq!(
        validate(ToolName::Grep, json!({ "pattern": "x", "ignoreCase": "TRUE", "literal": "1" })).unwrap(),
        json!({ "pattern": "x", "ignoreCase": true, "literal": true })
    );
}
