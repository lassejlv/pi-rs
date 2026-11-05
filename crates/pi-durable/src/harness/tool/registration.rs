//! Executable registrations are host capabilities; persisted declarations stay JSON.
use super::*;
use crate::harness::{
    declaration::{DeclaredTool, ToolDeclaration},
    registry::{RegisteredTool, ToolCaller},
};
use crate::{storage::StorageFuture, *};
use pi_chord::output::OutputLimits;
use std::sync::Arc;

pub trait ExecutableTool: Send + Sync + 'static {
    fn declaration(&self) -> ToolDeclaration;
    fn replay(&self) -> ReplayPolicy {
        ReplayPolicy::Unsafe
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }
    fn output_limits(&self) -> OutputLimits {
        OutputLimits {
            max_bytes: 50 * 1024,
            max_lines: 2000,
            retain: pi_chord::output::Retain::Head,
        }
    }
    fn structured_output_schema(&self) -> Option<JsonValue> {
        None
    }
    fn prepare_arguments(&self, arguments: JsonValue) -> Result<JsonValue> {
        Ok(arguments)
    }
    fn validate_arguments(&self, call: &ToolCall, arguments: JsonValue) -> Result<JsonObject> {
        validate_tool_arguments(&self.declaration(), call, arguments)
    }
    fn structured_output_error(&self, result: &ToolExecutionResult) -> Result<Option<JsonString>> {
        structured_output_error(
            &self.declaration().name,
            self.structured_output_schema().as_ref(),
            result,
        )
    }
    fn execute(
        &self,
        arguments: JsonObject,
        api: ToolExecutionApi,
        context: Context,
    ) -> StorageFuture<ToolExecutionResult>;
}

pub type ExecuteTool = Arc<
    dyn Fn(JsonObject, ToolExecutionApi, Context) -> StorageFuture<ToolExecutionResult>
        + Send
        + Sync,
>;
pub type PrepareArguments = Arc<dyn Fn(JsonValue) -> Result<JsonValue> + Send + Sync>;
pub type ValidateArguments =
    Arc<dyn Fn(&ToolDeclaration, &ToolCall, JsonValue) -> Result<JsonObject> + Send + Sync>;

#[derive(Clone)]
pub struct NativeTool {
    pub declaration: ToolDeclaration,
    /// Application metadata carried with the registration, such as a prompt snippet. It is never
    /// model-visible or persisted; sections and hooks read it from the resolved agent's tools.
    pub metadata: Option<JsonObject>,
    pub callers: Option<Vec<ToolCaller>>,
    pub deferred: bool,
    pub replay: ReplayPolicy,
    pub execution_mode: Option<ToolExecutionMode>,
    pub output_limits: OutputLimits,
    pub structured_output_schema: Option<JsonValue>,
    pub prepare_arguments: Option<PrepareArguments>,
    /// A registration with a lossless input representation can decode it natively.
    pub validate_arguments: Option<ValidateArguments>,
    pub execute: ExecuteTool,
}
impl NativeTool {
    pub fn with_metadata(mut self, metadata: JsonObject) -> Self {
        self.metadata = Some(metadata);
        self
    }
    pub fn new(declaration: ToolDeclaration, execute: ExecuteTool) -> Self {
        Self {
            declaration,
            metadata: None,
            callers: None,
            deferred: false,
            replay: ReplayPolicy::Unsafe,
            execution_mode: None,
            output_limits: OutputLimits {
                max_bytes: 50 * 1024,
                max_lines: 2000,
                retain: pi_chord::output::Retain::Head,
            },
            structured_output_schema: None,
            prepare_arguments: None,
            validate_arguments: None,
            execute,
        }
    }
}
impl RegisteredTool for NativeTool {
    fn name(&self) -> &JsonString {
        &self.declaration.name
    }
    fn callers(&self) -> Option<&[ToolCaller]> {
        self.callers.as_deref()
    }
    fn deferred(&self) -> bool {
        self.deferred
    }
    fn executable(self: Arc<Self>) -> Option<Arc<dyn ExecutableTool>> {
        Some(self)
    }
}
impl DeclaredTool for NativeTool {
    fn declaration(&self) -> ToolDeclaration {
        self.declaration.clone()
    }
}
impl ExecutableTool for NativeTool {
    fn declaration(&self) -> ToolDeclaration {
        self.declaration.clone()
    }
    fn replay(&self) -> ReplayPolicy {
        self.replay
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.execution_mode
    }
    fn output_limits(&self) -> OutputLimits {
        self.output_limits
    }
    fn structured_output_schema(&self) -> Option<JsonValue> {
        self.structured_output_schema.clone()
    }
    fn prepare_arguments(&self, arguments: JsonValue) -> Result<JsonValue> {
        self.prepare_arguments
            .as_ref()
            .map_or(Ok(arguments.clone()), |prepare| prepare(arguments))
    }
    fn validate_arguments(&self, call: &ToolCall, arguments: JsonValue) -> Result<JsonObject> {
        if let Some(validate) = &self.validate_arguments {
            check_arguments_error(call)?;
            validate(&self.declaration, call, arguments)
        } else {
            validate_tool_arguments(&self.declaration, call, arguments)
        }
    }
    fn execute(
        &self,
        arguments: JsonObject,
        api: ToolExecutionApi,
        context: Context,
    ) -> StorageFuture<ToolExecutionResult> {
        (self.execute)(arguments, api, context)
    }
}

fn check_arguments_error(call: &ToolCall) -> Result<()> {
    if let Some(error) = call
        .extra
        .get(&JsonString::from("argumentsError"))
        .and_then(JsonValue::as_string)
        && !error.units().is_empty()
    {
        return Err(Error::host(
            "Error",
            result::join(
                [
                    "Invalid arguments for tool \"".into(),
                    call.name.clone(),
                    "\": ".into(),
                    error.clone(),
                ],
                "",
            ),
        ));
    }
    Ok(())
}
pub fn validate_tool_arguments(
    declaration: &ToolDeclaration,
    call: &ToolCall,
    arguments: JsonValue,
) -> Result<JsonObject> {
    check_arguments_error(call)?;
    if !arguments.is_object() {
        return Err(Error::message("Tool arguments must be an object"));
    }
    // Validation keeps JavaScript strings, including unpaired surrogates.
    let value = pi_ai::utils::validation::utf16::validate_tool_arguments(
        &declaration.name,
        &declaration.parameters,
        false,
        &arguments,
        None,
    )
    .map_err(|message| Error::host("Error", message))?;
    JsonObject::try_from_value(value).map_err(Error::message)
}
fn structured_output_error(
    name: &JsonString,
    schema: Option<&JsonValue>,
    result: &ToolExecutionResult,
) -> Result<Option<JsonString>> {
    let prefix = result::join(["Tool ".into(), name.clone()], "");
    let message = match (schema, result.structured_output.as_ref()) {
        (None, None) => return Ok(None),
        (None, Some(_)) => {
            " returned structuredOutput but declares no structuredOutputSchema".into()
        }
        (Some(_), None) if result.is_error == Some(true) => return Ok(None),
        (Some(_), None) => " returned no structuredOutput".into(),
        (Some(schema), Some(value)) => {
            let validator = pi_ai::utils::validation::utf16::SchemaValidator::new(schema);
            let value = value.copy_json()?;
            if validator.check(&value) {
                return Ok(None);
            }
            let errors = validator.errors(&value);
            let first = errors.first();
            // `instancePath.replace(/^\//, "").replace(/\//g, ".") || "root"`
            let path = first
                .map(|error| {
                    let units = error.instance_path.units();
                    JsonString::from_utf16(
                        units
                            .strip_prefix(&[0x2f])
                            .unwrap_or(units)
                            .iter()
                            .map(|&unit| if unit == 0x2f { 0x2e } else { unit })
                            .collect(),
                    )
                })
                .filter(|path| !path.units().is_empty())
                .unwrap_or_else(|| "root".into());
            result::join(
                [
                    " returned structuredOutput that does not match its schema: ".into(),
                    path,
                    ": ".into(),
                    first.map_or_else(|| "invalid".into(), |error| error.message.clone()),
                ],
                "",
            )
        }
    };
    Ok(Some(result::join([prefix, message], "")))
}
