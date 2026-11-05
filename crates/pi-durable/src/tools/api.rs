//! Built-ins need I/O, progress and model metadata, independently of transport.
use super::image_processor::{DEFAULT_IMAGE_LIMITS, ImageLimits, ImageModel};
use crate::{
    env::*,
    harness::tool::{RetainedOutput, ToolExecutionApi},
    storage::StorageFuture,
    *,
};
use std::sync::Arc;
pub trait CodingToolApi: Send + Sync {
    fn environment(&self) -> Result<Option<Arc<dyn ExecutionEnv>>>;
    fn output(&self, text: JsonString, skipped: Option<ShellOutputSkip>) -> Result<()>;
    fn retained_output(&self) -> Result<RetainedOutput>;
    fn diagnostic(&self, diagnostic: harness::live::ToolDiagnostic) -> Result<()>;
    fn output_window(&self) -> Result<Option<ShellOutputWindow>>;
    fn image_model(&self, context: &Context) -> StorageFuture<ImageModel>;
}
impl CodingToolApi for ToolExecutionApi {
    fn environment(&self) -> Result<Option<Arc<dyn ExecutionEnv>>> {
        Ok(self.env.clone())
    }
    fn output(&self, text: JsonString, skipped: Option<ShellOutputSkip>) -> Result<()> {
        self.output(text, skipped)
    }
    fn retained_output(&self) -> Result<RetainedOutput> {
        self.retained_output()
    }
    fn diagnostic(&self, diagnostic: harness::live::ToolDiagnostic) -> Result<()> {
        self.diagnostic(diagnostic)
    }
    fn output_window(&self) -> Result<Option<ShellOutputWindow>> {
        Ok(self.output_window.as_ref().map(|w| ShellOutputWindow {
            max_bytes: w.max_bytes,
            max_lines: w.max_lines,
            min_interval_ms: w.min_interval_ms,
            bytes_per_second: w.bytes_per_second as f64,
        }))
    }
    fn image_model(&self, context: &Context) -> StorageFuture<ImageModel> {
        let agent = self.agent(context);
        let api = self.clone();
        Box::pin(async move {
            let agent = agent.await?;
            let Some(reference) = agent.model() else {
                return Ok(ImageModel::default());
            };
            let reference = wire::decode(reference).map_err(Error::message)?;
            let Some(model) = api.model_collection()?.get_model(&reference)? else {
                return Ok(ImageModel::default());
            };
            let image = model.image_input()?;
            Ok(ImageModel {
                vision: image.vision,
                limits: ImageLimits {
                    max_width: image.max_width.unwrap_or(DEFAULT_IMAGE_LIMITS.max_width),
                    max_height: image.max_height.unwrap_or(DEFAULT_IMAGE_LIMITS.max_height),
                    max_bytes: image.max_bytes.unwrap_or(DEFAULT_IMAGE_LIMITS.max_bytes),
                },
            })
        })
    }
}
