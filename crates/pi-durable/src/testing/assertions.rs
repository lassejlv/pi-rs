//! Assertion helpers for the runner-independent suites. Failures are returned
//! as errors so a runner can attribute them to a case.

use crate::{Error, JsonValue, Result};
use std::{fmt::Debug, future::Future};

/// Native counterpart of the upstream `StorageConformanceAssertions` object.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConformanceAssertions;

/// Adapts the native assertion helpers; the upstream function wraps a JS `expect`.
pub fn create_expect_assertions() -> ConformanceAssertions {
    ConformanceAssertions
}

fn subset(actual: &JsonValue, expected: &JsonValue) -> bool {
    match (actual.as_object(), expected.as_object()) {
        (Some(actual), Some(expected)) => expected
            .iter()
            .all(|(key, want)| actual.get(key).is_some_and(|have| subset(have, want))),
        _ => actual == expected,
    }
}

impl ConformanceAssertions {
    pub fn ok(&self, value: bool, message: &str) -> Result<()> {
        if value {
            Ok(())
        } else {
            Err(Error::message(format!("Assertion failed: {message}")))
        }
    }
    pub fn strict_equal<T: PartialEq + Debug + ?Sized>(
        &self,
        actual: &T,
        expected: &T,
    ) -> Result<()> {
        if actual == expected {
            Ok(())
        } else {
            Err(Error::message(format!(
                "Expected {expected:?}, got {actual:?}"
            )))
        }
    }
    pub fn deep_equal<T: PartialEq + Debug + ?Sized>(
        &self,
        actual: &T,
        expected: &T,
    ) -> Result<()> {
        self.strict_equal(actual, expected)
    }
    /// Every property of `expected` must be present, recursively, in `actual`.
    pub fn partial_deep_equal(&self, actual: &JsonValue, expected: &JsonValue) -> Result<()> {
        if subset(actual, expected) {
            Ok(())
        } else {
            Err(Error::message(format!(
                "Expected {actual} to match {expected}"
            )))
        }
    }
    pub fn greater_than(&self, actual: f64, expected: f64) -> Result<()> {
        if actual > expected {
            Ok(())
        } else {
            Err(Error::message(format!(
                "Expected {actual} to be greater than {expected}"
            )))
        }
    }
    pub async fn rejects<T>(
        &self,
        operation: impl Future<Output = Result<T>>,
        message_includes: &str,
    ) -> Result<()> {
        match operation.await {
            Ok(_) => Err(Error::message("Expected the operation to reject")),
            Err(error) if error.to_string().contains(message_includes) => Ok(()),
            Err(error) => Err(Error::message(format!(
                "Expected rejection containing {message_includes:?}, got {error}"
            ))),
        }
    }
}
