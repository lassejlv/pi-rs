//! Portable environment capabilities. The filesystem operations used by durable
//! storage share the same authority and error model as later tool environments.
pub mod decode;
pub mod filesystem;
pub mod line_scan;
#[cfg(not(target_arch = "wasm32"))]
pub mod list_files;
#[cfg(not(target_arch = "wasm32"))]
pub mod native;
#[cfg(not(target_arch = "wasm32"))]
pub mod native_shell;
#[cfg(not(target_arch = "wasm32"))]
pub mod native_watch;
pub mod readers;
pub mod shell;
pub use filesystem::*;
pub use line_scan::{LineScan, LineScanner};
#[cfg(not(target_arch = "wasm32"))]
pub use native::{NativeExecutionEnv, NativeFileSystem};
#[cfg(not(target_arch = "wasm32"))]
pub use native_shell::{
    NativeShellOptions, ShellHost, ShellPlatform, ShellSpillOpener, ShellSpillWriter, ShellTempFile,
};
#[cfg(not(target_arch = "wasm32"))]
pub use native_watch::{
    NativeFileWatcher, NativeWatchCallback, NativeWatchEvent, NativeWatchHandle, NativeWatchOpener,
    NativeWatchOptions,
};
pub use shell::*;
