use super::{
    check_aborted, file_error, file_mutation_queue::with_file_mutation_queue,
    path_utils::resolve_tool_path, require_env,
};
use crate::{
    Context, JsonString, Result,
    env::ExecutionEnv,
    harness::{
        declaration::ToolDeclaration,
        tool::{NativeTool, ToolExecutionResult, text_content},
    },
    storage::{StorageFuture, line::eager},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WriteToolInput {
    pub path: JsonString,
    pub content: JsonString,
}

pub fn execute_write(
    args: WriteToolInput,
    env: Option<Arc<dyn ExecutionEnv>>,
    context: Context,
) -> StorageFuture<ToolExecutionResult> {
    eager(async move {
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
                    env.write_text_file(&absolute, &args.content, &context)
                        .await
                        .map_err(file_error)?;
                    check_aborted(&context)?;
                    let mut text = JsonString::from("Successfully wrote to ");
                    text.append(&args.path);
                    Ok(ToolExecutionResult {
                        output: Some(Arc::new(vec![text_content(text)])),
                        ..Default::default()
                    })
                })
            },
            queued_context,
        )
        .await
    })
}
pub fn create_write_tool() -> NativeTool {
    let parameters = crate::wire::encode(
        &serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Path to the file to write (relative or absolute)"},"content":{"type":"string","description":"Content to write to the file"}},"required":["path","content"]}),
    );
    NativeTool::new(
        ToolDeclaration::new(
            "write",
            "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
            parameters,
        ),
        Arc::new(|args, api, context| {
            let parsed: Result<WriteToolInput> =
                crate::wire::decode(args.value()).map_err(crate::Error::message);
            match parsed {
                Ok(args) => execute_write(args, api.env.clone(), context),
                Err(error) => Box::pin(async { Err(error) }),
            }
        }),
    )
}
