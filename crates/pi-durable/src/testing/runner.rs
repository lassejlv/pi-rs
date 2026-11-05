//! Runs the conformance suites against a provider and reports every failure.

use super::{
    EnvConformanceOptions, EnvConformanceProvider, StorageBackend, StorageConformanceProvider,
    create_env_conformance, create_storage_conformance_for,
};
use crate::{Error, Result};
use std::time::Duration;

const DEFAULT_CASE_TIMEOUT: Duration = Duration::from_secs(10);

fn summarize(name: &str, total: usize, failures: Vec<String>) -> Result<()> {
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::message(format!(
            "{name}: {} of {total} conformance cases failed:\n{}",
            failures.len(),
            failures.join("\n")
        )))
    }
}

/// Runs every storage case with a fresh storage from `with_storage`.
pub async fn run_storage_conformance(
    name: &str,
    backend: StorageBackend,
    with_storage: &StorageConformanceProvider,
) -> Result<()> {
    let cases = create_storage_conformance_for(backend)?;
    let mut failures = Vec::new();
    for case in &cases {
        let run = case.run_with(with_storage);
        match pi_chord::rt::timeout(DEFAULT_CASE_TIMEOUT, run).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(format!("  {}: {error}", case.name)),
            Err(_) => failures.push(format!("  {}: timed out", case.name)),
        }
    }
    summarize(name, cases.len(), failures)
}

/// Runs every `ExecutionEnv` case with a fresh environment from `with_env`.
pub async fn run_env_conformance(
    name: &str,
    with_env: &EnvConformanceProvider,
    options: EnvConformanceOptions,
) -> Result<()> {
    let cases = create_env_conformance(with_env.clone(), options);
    let mut failures = Vec::new();
    for case in &cases {
        let limit = case.timeout.unwrap_or(DEFAULT_CASE_TIMEOUT);
        match pi_chord::rt::timeout(limit, case.run()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(format!("  {}: {error}", case.name)),
            Err(_) => failures.push(format!("  {}: timed out", case.name)),
        }
    }
    summarize(name, cases.len(), failures)
}
