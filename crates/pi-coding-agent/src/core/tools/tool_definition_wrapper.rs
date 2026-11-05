//! Port of `packages/coding-agent/src/core/tools/tool-definition-wrapper.ts`.

use crate::core::extensions::types::ExtensionToolContext;
use crate::core::extensions::types::ToolDefinition;
use pi_agent::types::{AgentTool, AgentToolRef, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use pi_ai::types::{ConstrainedSampling, JsonObject};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Creates the context for one tool call.
pub type ToolContextFactory =
    Arc<dyn Fn(String, Option<CancellationToken>) -> ExtensionToolContext + Send + Sync>;

/// An [`AgentTool`] backed by a [`ToolDefinition`] (the object `wrapToolDefinition` returns).
pub struct DefinitionAgentTool {
    definition: ToolDefinition,
    constrained_sampling: Option<ConstrainedSampling>,
    ctx_factory: Option<ToolContextFactory>,
}

impl DefinitionAgentTool {
    /// The wrapped definition.
    pub fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
}

#[async_trait::async_trait]
impl AgentTool for DefinitionAgentTool {
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn label(&self) -> &str {
        &self.definition.label
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn parameters(&self) -> &Value {
        &self.definition.parameters
    }

    fn parameters_typebox(&self) -> bool {
        self.definition.parameters_typebox
    }

    fn constrained_sampling(&self) -> Option<&ConstrainedSampling> {
        self.constrained_sampling.as_ref()
    }

    fn prepare_arguments(&self, args: &JsonObject) -> Option<JsonObject> {
        let prepare = self.definition.prepare_arguments.as_ref()?;
        match prepare(Value::Object(args.clone())) {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    fn output_schema(&self) -> Option<&Value> {
        self.definition.output_schema.as_ref()
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.definition.execution_mode
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        on_update: AgentToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let ctx = self
            .ctx_factory
            .as_ref()
            .map(|factory| factory(tool_call_id.to_string(), Some(signal.clone())));
        (self.definition.execute)(tool_call_id.to_string(), params, Some(signal), Some(on_update), ctx).await
    }
}

/// Wrap a ToolDefinition into an AgentTool for the core runtime.
pub fn wrap_tool_definition(
    definition: &ToolDefinition,
    ctx_factory: Option<&ToolContextFactory>,
) -> AgentToolRef {
    Arc::new(DefinitionAgentTool {
        definition: definition.clone(),
        constrained_sampling: definition
            .constrained_sampling
            .clone()
            .map(ConstrainedSampling::Config),
        ctx_factory: ctx_factory.cloned(),
    })
}

/// Wrap multiple ToolDefinitions into AgentTools for the core runtime.
pub fn wrap_tool_definitions(
    definitions: &[ToolDefinition],
    ctx_factory: Option<&ToolContextFactory>,
) -> Vec<AgentToolRef> {
    definitions
        .iter()
        .map(|definition| wrap_tool_definition(definition, ctx_factory))
        .collect()
}

/// Synthesize a minimal ToolDefinition from an AgentTool.
///
/// This keeps AgentSession's internal registry definition-first even when a caller
/// provides plain AgentTool overrides that do not include prompt metadata or renderers.
pub fn create_tool_definition_from_agent_tool(tool: AgentToolRef) -> ToolDefinition {
    let execute_tool = tool.clone();
    let mut definition = ToolDefinition::new(
        tool.name().to_string(),
        tool.label().to_string(),
        tool.description().to_string(),
        tool.parameters().clone(),
        move |tool_call_id, params, signal, on_update, _ctx| {
            let tool = execute_tool.clone();
            async move {
                let signal = signal.unwrap_or_default();
                let on_update = on_update.unwrap_or_else(|| Arc::new(|_| {}));
                tool.execute(&tool_call_id, params, signal, on_update).await
            }
        },
    );
    definition.output_schema = tool.output_schema().cloned();
    definition.parameters_typebox = tool.parameters_typebox();
    definition.constrained_sampling = tool.constrained_sampling().and_then(|c| c.config().cloned());
    definition.execution_mode = tool.execution_mode();
    let prepare_tool = tool.clone();
    definition.prepare_arguments = Some(Arc::new(move |args: Value| match &args {
        Value::Object(map) => prepare_tool.prepare_arguments(map).map(Value::Object).unwrap_or(args),
        _ => args,
    }));
    definition
}
