//! Provider and case types shared by the conformance suites.

use crate::{Result, env::ExecutionEnv, storage::Storage};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

pub use super::{
    StorageConformanceCase, assertions::ConformanceAssertions as StorageConformanceAssertions,
};

/// The assertion helper shared by the storage and environment suites.
pub type EnvConformanceAssertions = StorageConformanceAssertions;

pub type ConformanceFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;

/// Body run against a provided storage.
pub type StorageUse = Box<dyn FnOnce(Arc<dyn Storage>) -> ConformanceFuture + Send>;
/// Calls the body once with a fresh storage, then cleans up.
pub type StorageConformanceProvider = Arc<dyn Fn(StorageUse) -> ConformanceFuture + Send + Sync>;

/// Body run against a provided environment.
pub type EnvUse = Box<dyn FnOnce(Arc<dyn ExecutionEnv>) -> ConformanceFuture + Send>;
/// Calls the body once with an environment whose `cwd` is a fresh, empty,
/// writable directory, then cleans up.
pub type EnvConformanceProvider = Arc<dyn Fn(EnvUse) -> ConformanceFuture + Send + Sync>;

#[derive(Clone)]
pub struct EnvConformanceOptions {
    /// Program and flag that run a POSIX shell script from the next argument; default `["sh", "-c"]`.
    pub shell: Vec<String>,
    /// Whether the shell's `ln -s` creates symbolic links; default true.
    pub symlinks: bool,
    /// Whether the environment implements the optional `list_files`, which the suite then checks; default false.
    pub list_files: bool,
}
impl Default for EnvConformanceOptions {
    fn default() -> Self {
        Self {
            shell: vec!["sh".into(), "-c".into()],
            symlinks: true,
            list_files: false,
        }
    }
}

pub struct EnvConformanceCase {
    pub name: &'static str,
    /// Cases that wait for an environment's watch latency need longer than the default.
    pub timeout: Option<Duration>,
    pub(crate) run: Arc<dyn Fn() -> ConformanceFuture + Send + Sync>,
}
impl EnvConformanceCase {
    pub fn run(&self) -> ConformanceFuture {
        (self.run)()
    }
}
