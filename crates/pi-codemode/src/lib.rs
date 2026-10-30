//! # pi-codemode
//!
//! Runs model-written JavaScript in a sandboxed VM where the only capability is calling injected
//! tools. Nested tool calls never enter the LLM context; only the script's output and return value
//! do. Port of `@earendil-works/pi-codemode`.
//!
//! Scripts use `tools`, `ALL_TOOLS`, `text`, `image`, `exit`, `store`, and `load`, and may start
//! with a `// @options:` line (see [`parse_codemode_source`]). The VM is [Boa](https://boajs.dev),
//! the engine behind `pi-jsrt`, run directly on a dedicated thread per execution with nothing but
//! ECMAScript built-ins, `performance.now()`, and the codemode prelude.
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use pi_codemode::{CodemodeExecuteOptions, CodemodeSandbox, CodemodeSandboxOptions, CodemodeTool};
//! use serde_json::json;
//!
//! let sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
//!     timeout_ms: Some(60_000.0),
//!     tools: vec![CodemodeTool::new("read", |args, _ctx| async move {
//!         let path = args.and_then(|a| a["path"].as_str().map(str::to_owned)).unwrap_or_default();
//!         std::fs::read_to_string(path).map(|text| Some(json!(text))).map_err(|e| e.to_string())
//!     })],
//!     ..Default::default()
//! })?;
//! let result = sandbox
//!     .execute(
//!         r#"const source = await tools.read({ path: "Cargo.toml" }); text("bytes " + source.length); return 1;"#,
//!         CodemodeExecuteOptions::default(),
//!     )
//!     .await?;
//! assert!(result.ok());
//! sandbox.close().await;
//! # Ok(()) }
//! ```
//!
//! Differences from the TS package (QuickJS in wasm) are listed in `PORTING-STATUS.md`. Boa cannot
//! be interrupted from outside, so the script's loops are instrumented to poll the interrupt flag
//! and count the interrupt budget; and there is no heap limit.

pub mod abort;
pub mod declarations;
pub mod identifier;
pub mod js;
pub mod json_run;
pub mod runtime;
pub mod source;
pub mod types;

pub use abort::{AbortController, AbortSignal};
pub use declarations::{
    DEFAULT_INPUT_SCHEMA_MAX_CHARS, RenderDeclarationsOptions, render_declarations, render_tool_output_type,
    render_tool_sample, render_tool_signature, schema_to_type,
};
pub use declarations::json::{
    JsonToolDeclaration, render_json_declarations, render_json_tool_output_type, render_json_tool_sample,
    render_json_tool_signature, schema_to_json_type, to_json_codemode_identifier,
};
pub use identifier::to_codemode_identifier;
pub use json_run::*;
pub use runtime::execution::{
    CodemodeSandbox, CodemodeSandboxBase, CodemodeSandboxBaseOptions, CodemodeSandboxError, DEFAULT_TIMEOUT_MS, VmChannel,
    VmEvents, VmLauncher,
};
pub use runtime::host::{AcquireWorker, CodemodeSandboxOptions, OnWorkerStarted, ThreadLauncher, WorkerExited, WorkerRelease};
pub use runtime::prelude_source::{MAX_OUTPUT_CHARS, MAX_OUTPUT_ITEMS, MAX_STORE_TOTAL_CHARS, MAX_STORE_VALUE_CHARS, PRELUDE_SOURCE};
pub use runtime::protocol::{DoneMessage, HostToWorkerMessage, VmDeterministic, VmGlobal, VmStartData, VmTool, WorkerToHostMessage};
pub use runtime::remote::{
    CodemodeRemote, CodemodeRemoteExchange, CodemodeRemoteFactory, CodemodeRemoteMessage, CodemodeRemoteReply,
    CodemodeRemoteRequest, CodemodeRemoteServer, CodemodeServeOptions, DEFAULT_INTERRUPT_BUDGET, RemoteCodemodeSandbox,
    RemoteCodemodeSandboxOptions, RemoteLauncher, serve_codemode_remote,
};
pub use runtime::vm::{DEFAULT_LOOP_ITERATION_LIMIT, DEFAULT_RECURSION_LIMIT, VmLimits};
pub use source::{
    CODEMODE_OPTIONS_PREFIX, CODEMODE_SOURCE_GRAMMAR, CodemodeSourceError, CodemodeSourceOptions, ParsedCodemodeSource,
    parse_codemode_source, parse_codemode_source_utf16, ParsedCodemodeSourceUtf16, CodemodeSourceErrorUtf16,
};
pub use types::*;

/// The sandboxes and types that need no particular host (`@earendil-works/pi-codemode/portable`).
/// In Rust every entry point is portable; this module only mirrors the TS entry.
pub mod portable {
    pub use crate::json_run::*;
    pub use crate::runtime::execution::{CodemodeSandbox, CodemodeSandboxBase};
    pub use crate::runtime::prelude_source::{MAX_OUTPUT_CHARS, MAX_OUTPUT_ITEMS, MAX_STORE_TOTAL_CHARS, MAX_STORE_VALUE_CHARS};
    pub use crate::runtime::protocol::{HostToWorkerMessage, VmStartData, WorkerToHostMessage};
    pub use crate::runtime::remote::*;
    pub use crate::to_codemode_identifier;
    pub use crate::types::*;
}
