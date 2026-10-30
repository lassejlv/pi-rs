//! # pi-durable-jsrt — embedded `@earendil-works/pi-durable` and `@earendil-works/chord`
//!
//! The upstream packages' `exports` subpaths served as [`pi_jsrt`] virtual modules (`js/`), whose
//! glue forwards to native `pi-durable` / `pi-chord` through in-process host methods
//! ([`host`]). Values cross as [`HostValue`]s converted directly from Boa values (UTF-16
//! lossless, explicit `undefined`, functions and objects as handles, see [`value`]); failures
//! cross as [`HostError`]s reconstructed as the upstream error classes ([`errors`]).
//!
//! ```no_run
//! # async fn demo() -> pi_jsrt::Result<()> {
//! let rt = pi_jsrt::JsRuntime::new(pi_jsrt::RuntimeOptions::default())?;
//! let host = pi_durable_jsrt::install(&rt, pi_durable_jsrt::HostOptions::default()).await?;
//! let ns = rt.eval_module("export { boundOutput } from '@earendil-works/chord/output';", "main.mjs").await?;
//! # let _ = (host, ns); Ok(()) }
//! ```
//!
//! The `vitest` shim (`js/vitest.js`, [`HostOptions::vitest`]) and the `embedded_stock` example
//! run unchanged upstream test files inside the runtime against these modules.

#![warn(missing_docs)]

pub mod dispatch;
pub mod errors;
pub mod hoist;
pub mod host;
pub mod value;

pub use errors::{ErrorClass, HostError, HostResult, Thrown, TypedError};
pub use hoist::hoist_vitest_mocks;
pub use host::{
    Args, BRIDGE, Call, Dispatch, DurableHost, GLOBAL, HostOptions, Methods, call_async, call_sync,
    install, modules, specifier_for, virtual_modules,
};
pub use value::{HostValue, NativeRef, from_js, strict_from_js, strict_to_js, to_js};
