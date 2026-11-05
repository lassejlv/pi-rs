//! Built-in coding tools use the invocation's execution environment.
pub mod edit;
pub mod edit_diff;
pub mod file_mutation_queue;
pub use edit::{EditToolDetails, EditToolInput, create_edit_tool};
pub mod api;
pub mod bash;
pub mod image;
pub mod image_processor;
pub mod path_utils;
pub mod read;
pub mod write;
pub use bash::{
    BashExecution, BashPrepare, BashToolInput, BashToolOptions, PowerShellToolInput,
    PowerShellToolOptions, create_bash_tool, create_powershell_tool,
};
pub use image_processor::{DEFAULT_IMAGE_LIMITS, ImageLimits, ImageProcessor, PreparedImage};
pub use read::{ReadToolInput, ReadToolOptions, create_read_tool};
pub use write::{WriteToolInput, create_write_tool};
pub fn create_coding_tools<S>(
    options: read::ReadToolOptions,
) -> crate::harness::registry::Extension<crate::harness::tool::NativeTool, S> {
    let mut extension = crate::harness::registry::Extension::new("coding-tools");
    extension.tools = vec![
        Arc::new(create_read_tool(options)),
        Arc::new(create_write_tool()),
        Arc::new(create_edit_tool()),
        Arc::new(create_bash_tool(Default::default())),
    ];
    extension
}
pub fn coding_tools<S>() -> crate::harness::registry::Extension<crate::harness::tool::NativeTool, S>
{
    create_coding_tools(Default::default())
}

use crate::{Context, Error, Result, env::ExecutionEnv};
use std::sync::Arc;

pub fn require_env(env: Option<Arc<dyn ExecutionEnv>>) -> Result<Arc<dyn ExecutionEnv>> {
    env.ok_or_else(|| Error::message("No execution environment is configured"))
}
pub(crate) fn check_aborted(context: &Context) -> Result<()> {
    if context.check().is_err() {
        Err(Error::message("Operation aborted"))
    } else {
        Ok(())
    }
}
pub(crate) fn file_error(error: crate::env::FileError) -> Error {
    Error::file(error)
}
