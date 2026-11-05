use super::{
    check_aborted, edit_diff::*, file_error, file_mutation_queue::with_file_mutation_queue,
    path_utils::resolve_tool_path, require_env,
};
use crate::{
    Context, Error, JsonString, JsonValue,
    env::{ExecutionEnv, FileKind},
    harness::{
        declaration::ToolDeclaration,
        tool::{NativeTool, ToolExecutionResult, text_content},
    },
    storage::{StorageFuture, line::eager},
    wire,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditToolInput {
    pub path: JsonString,
    pub edits: Vec<Edit>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditToolDetails {
    pub diff: JsonString,
    pub patch: JsonString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<usize>,
}
fn single(value: &JsonValue) -> bool {
    value.is_object() && value["oldText"].is_string() && value["newText"].is_string()
}
pub fn prepare_edit_arguments(input: JsonValue) -> JsonValue {
    let JsonValue::Object(mut args) = input else {
        return input;
    };
    if let Some(edits) = Arc::make_mut(&mut args).get_mut(&JsonString::from("edits")) {
        if let Some(text) = edits.as_string() {
            // Escaping unpaired units preserves JSON.parse's lossless strings.
            let source = char::decode_utf16(text.units().iter().copied())
                .map(|c| match c {
                    Ok(c) => c.to_string(),
                    Err(e) => format!("\\u{:04x}", e.unpaired_surrogate()),
                })
                .collect::<String>();
            if let Ok(parsed) = pi_chord::json::parse(&source) {
                if parsed.is_array() {
                    *edits = parsed;
                } else if single(&parsed) {
                    *edits = JsonValue::Array(vec![parsed].into());
                }
            }
        } else if single(edits) {
            *edits = JsonValue::Array(vec![edits.clone()].into());
        }
    }
    if args
        .get(&JsonString::from("oldText"))
        .is_some_and(JsonValue::is_string)
        && args
            .get(&JsonString::from("newText"))
            .is_some_and(JsonValue::is_string)
    {
        let mut edits = args
            .get(&JsonString::from("edits"))
            .and_then(JsonValue::as_array)
            .cloned()
            .unwrap_or_default();
        let mut edit = pi_chord::json::Map::new();
        edit.insert(
            "oldText".into(),
            Arc::make_mut(&mut args)
                .shift_remove(&JsonString::from("oldText"))
                .unwrap(),
        );
        edit.insert(
            "newText".into(),
            Arc::make_mut(&mut args)
                .shift_remove(&JsonString::from("newText"))
                .unwrap(),
        );
        edits.push(JsonValue::Object(edit.into()));
        Arc::make_mut(&mut args).insert("edits".into(), JsonValue::Array(edits.into()));
    }
    JsonValue::Object(args)
}
fn access(path: &JsonString, error: crate::env::FileError) -> Error {
    let code = match error.code() {
        crate::env::FileErrorCode::Aborted => "aborted",
        crate::env::FileErrorCode::NotFound => "not_found",
        crate::env::FileErrorCode::PermissionDenied => "permission_denied",
        crate::env::FileErrorCode::NotDirectory => "not_directory",
        crate::env::FileErrorCode::IsDirectory => "is_directory",
        crate::env::FileErrorCode::Invalid => "invalid",
        crate::env::FileErrorCode::NotSupported => "not_supported",
        crate::env::FileErrorCode::Unknown => "unknown",
    };
    let mut message = JsonString::from("Could not edit file: ");
    message.append(path);
    message.append(&format!(". Error code: {code}.").into());
    Error::caused(message, file_error(error))
}
pub fn execute_edit(
    args: JsonValue,
    env: Option<Arc<dyn ExecutionEnv>>,
    context: Context,
) -> StorageFuture<ToolExecutionResult> {
    eager(async move {
        if args["edits"]
            .as_array()
            .is_none_or(|edits| edits.is_empty())
        {
            return Err(Error::message(
                "Edit tool input is invalid. edits must contain at least one replacement.",
            ));
        }
        let args: EditToolInput = wire::decode(&args).map_err(Error::message)?;
        let env = require_env(env)?;
        let absolute =
            resolve_tool_path(env.as_ref(), &args.path.to_string_lossy(), &context).await?;
        let queued_context = context.clone();
        with_file_mutation_queue(
            env.clone(),
            absolute.clone(),
            move || {
                Box::pin(async move {
                    check_aborted(&context)?;
                    let info = env
                        .file_info(&absolute, &context)
                        .await
                        .map_err(|e| access(&args.path, e))?;
                    if !matches!(info.kind, FileKind::File | FileKind::Symlink) {
                        let mut text = JsonString::from("Could not edit file: ");
                        text.append(&args.path);
                        text.append(&". Path is not a file.".into());
                        return Err(Error::host("Error", text));
                    }
                    let content = env
                        .read_text_file(&absolute, &context)
                        .await
                        .map_err(|e| access(&args.path, e))?;
                    check_aborted(&context)?;
                    let (mut bom, content) = strip_bom(&content.into());
                    let ending = detect_line_ending(&content);
                    let applied = apply_edits_to_normalized_content(
                        &normalize_to_lf(&content),
                        &args.edits,
                        &args.path,
                    )?;
                    check_aborted(&context)?;
                    bom.append(&restore_line_endings(&applied.new_content, ending));
                    env.write_text_file(&absolute, &bom, &context)
                        .await
                        .map_err(|e| access(&args.path, e))?;
                    check_aborted(&context)?;
                    let diff = generate_diff_string(&applied.base_content, &applied.new_content, 4);
                    let details = EditToolDetails {
                        diff: diff.diff,
                        patch: generate_unified_patch(
                            &args.path,
                            &applied.base_content,
                            &applied.new_content,
                            4,
                        ),
                        first_changed_line: diff.first_changed_line,
                    };
                    let mut text = JsonString::from(format!(
                        "Successfully replaced {} block(s) in ",
                        args.edits.len()
                    ));
                    text.append(&args.path);
                    text.append(&".".into());
                    Ok(ToolExecutionResult {
                        output: Some(Arc::new(vec![text_content(text)])),
                        details: Some(wire::encode(&details).into()),
                        ..Default::default()
                    })
                })
            },
            queued_context,
        )
        .await
    })
}
pub fn create_edit_tool() -> NativeTool {
    let schema = wire::encode(
        &serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Path to the file to edit (relative or absolute)"},"edits":{"type":"array","description":"One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.","items":{"type":"object","properties":{"oldText":{"type":"string","description":"Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},"newText":{"type":"string","description":"Replacement text for this targeted edit."}},"required":["oldText","newText"]}}},"required":["path","edits"]}),
    );
    let mut tool = NativeTool::new(
        ToolDeclaration::new(
            "edit",
            "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.",
            schema,
        ),
        Arc::new(|args, api, context| execute_edit(args.into_value(), api.env.clone(), context)),
    );
    tool.prepare_arguments = Some(Arc::new(|args| Ok(prepare_edit_arguments(args))));
    tool
}
