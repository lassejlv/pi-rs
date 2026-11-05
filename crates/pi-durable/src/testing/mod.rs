//! Backend-independent request runner used by the reference conformance bridge
//! and native tests of independently generated reference traces.

pub mod assertions;
pub mod env_conformance;
pub mod runner;
pub mod storage_benchmark;
pub mod types;
pub use assertions::{ConformanceAssertions, create_expect_assertions};
pub use env_conformance::create_env_conformance;
#[cfg(not(target_arch = "wasm32"))]
pub use env_conformance::native_env_provider;
pub use runner::{run_env_conformance, run_storage_conformance};
pub use storage_benchmark::*;
pub use types::{
    ConformanceFuture, EnvConformanceAssertions, EnvConformanceCase, EnvConformanceOptions,
    EnvConformanceProvider, EnvUse, StorageConformanceProvider, StorageUse,
};

use crate::{
    storage::{Adopt, Storage},
    wire, *,
};
use serde::Serialize;
use std::sync::Arc;

fn arg<T: serde::de::DeserializeOwned>(args: &[JsonValue], index: usize) -> Result<T> {
    wire::decode(
        args.get(index)
            .ok_or_else(|| Error::request("Missing storage argument"))?,
    )
    .map_err(Error::request)
}
fn optional<T: serde::de::DeserializeOwned>(args: &[JsonValue], index: usize) -> Result<Option<T>> {
    match args.get(index) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(value) => wire::decode(value).map(Some).map_err(Error::request),
    }
}
fn encoded<T: Serialize>(value: T) -> Result<Option<JsonValue>> {
    Ok(Some(
        pi_chord::json::to_value(&value).map_err(Error::message)?,
    ))
}
fn maybe<T: Serialize>(value: Option<T>) -> Result<Option<JsonValue>> {
    value
        .map(|value| pi_chord::json::to_value(&value).map_err(Error::message))
        .transpose()
}
fn encoded_future<T: Serialize + Send + 'static>(
    future: crate::storage::StorageFuture<T>,
) -> crate::storage::StorageFuture<Option<JsonValue>> {
    Box::pin(async move { encoded(future.await?) })
}
fn maybe_future<T: Serialize + Send + 'static>(
    future: crate::storage::StorageFuture<Option<T>>,
) -> crate::storage::StorageFuture<Option<JsonValue>> {
    Box::pin(async move { maybe(future.await?) })
}
/// Invoke the backend at dispatch time, before returning its waiter. In particular,
/// a read started by adoption must start before a later close request is admitted.
pub fn storage_request(
    storage: &dyn Storage,
    method: &str,
    args: &[JsonValue],
    adopt: Adopt,
) -> crate::storage::StorageFuture<Option<JsonValue>> {
    let context = pi_chord::context::BACKGROUND_CONTEXT.clone();
    let begin = || -> Result<crate::storage::StorageFuture<Option<JsonValue>>> {
        Ok(match method {
            "commit" => {
                encoded_future(storage.commit(&arg::<Vec<StorageWrite>>(args, 0)?, adopt, &context))
            }
            "mintId" => encoded_future(storage.mint_id()),
            "conversation" => maybe_future(storage.conversation(arg(args, 0)?, &context)),
            "scanConversations" => encoded_future(storage.scan_conversations(
                &arg(args, 0)?,
                arg(args, 1)?,
                optional::<Cursor>(args, 2)?.as_ref(),
                &context,
            )),
            "entry" if args.len() == 1 => maybe_future(storage.entry(arg(args, 0)?, &context)),
            "entry" => maybe_future(storage.entry_in(arg(args, 0)?, arg(args, 1)?, &context)),
            "findLatestHeadMarker" => maybe_future(storage.find_latest_head_marker(
                arg(args, 0)?,
                optional(args, 1)?,
                &context,
            )),
            "scanEntries" => encoded_future(storage.scan_entries(
                &arg(args, 0)?,
                arg(args, 1)?,
                optional::<Cursor>(args, 2)?.as_ref(),
                &context,
            )),
            "task" => maybe_future(storage.task(arg(args, 0)?, &context)),
            "scanTasks" => encoded_future(storage.scan_tasks(
                &arg(args, 0)?,
                arg(args, 1)?,
                optional::<Cursor>(args, 2)?.as_ref(),
                &context,
            )),
            "submission" => maybe_future(storage.submission(arg(args, 0)?, &context)),
            "scanSubmissions" => encoded_future(storage.scan_submissions(
                &arg(args, 0)?,
                arg(args, 1)?,
                optional::<Cursor>(args, 2)?.as_ref(),
                &context,
            )),
            "submissionByRequest" => {
                maybe_future(storage.submission_by_request(arg(args, 0)?, &arg(args, 1)?, &context))
            }
            "findDocument" => {
                maybe_future(storage.find_document(&arg(args, 0)?, arg(args, 1)?, &context))
            }
            "document" => maybe_future(storage.document(arg(args, 0)?, arg(args, 1)?, &context)),
            "scanDocuments" => encoded_future(storage.scan_documents(
                &arg(args, 0)?,
                arg(args, 1)?,
                optional::<Cursor>(args, 2)?.as_ref(),
                &context,
            )),
            "close" => {
                let close = storage.close(&context);
                Box::pin(async move {
                    close.await?;
                    Ok(None)
                })
            }
            _ => return Err(Error::request(format!("Unknown storage method {method}"))),
        })
    };
    begin().unwrap_or_else(|error| Box::pin(async { Err(error) }))
}

pub async fn replay_case(storage: Arc<dyn Storage>, case: &JsonValue) -> Result<()> {
    for (index, request) in case["requests"]
        .as_array()
        .ok_or_else(|| Error::message("Missing requests"))?
        .iter()
        .enumerate()
    {
        let method = request["method"]
            .to_utf8()
            .ok_or_else(|| Error::message("Missing method"))?;
        let adopted = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let result = storage_request(
            storage.as_ref(),
            &method,
            request["args"]
                .as_array()
                .ok_or_else(|| Error::message("Missing args"))?,
            {
                let adopted = adopted.clone();
                Box::new(move |seq| adopted.lock().push(seq))
            },
        )
        .await;
        let expected_error = wire::get(request, "error");
        match (result, expected_error) {
            (Err(error), Some(expected)) => {
                let name = error.name();
                if !expected["name"].string_equals(name) {
                    return Err(Error::message(format!(
                        "Request {index} {method}: error kind {name}, expected {}",
                        expected["name"]
                    )));
                }
                if !expected["message"]
                    .as_string()
                    .is_some_and(|message| message.equals_str(&error.to_string()))
                {
                    return Err(Error::message(format!(
                        "Request {index} {method}: error {error}, expected {expected}"
                    )));
                }
            }
            (Ok(value), None) => {
                let expected = if request["missing"].as_bool() == Some(true) {
                    None
                } else {
                    wire::get(request, "value").cloned()
                };
                if value != expected {
                    return Err(Error::message(format!(
                        "Request {index} {method}: got {value:?}, expected {expected:?}"
                    )));
                }
            }
            (actual, expected) => {
                return Err(Error::message(format!(
                    "Request {index} {method}: result {actual:?}, expected error {expected:?}"
                )));
            }
        }
        if wire::encode(&*adopted.lock()) != request["adopted"] {
            return Err(Error::message(format!(
                "Request {index} {method}: adoption differs"
            )));
        }
    }
    Ok(())
}

pub struct StorageConformanceCase {
    pub name: JsonString,
    trace: JsonValue,
}
impl StorageConformanceCase {
    pub async fn run(&self, storage: Arc<dyn Storage>) -> Result<()> {
        replay_case(storage, &self.trace).await
    }
    /// Run against a fresh storage supplied by `provider`, which cleans up afterwards.
    pub fn run_with(&self, provider: &StorageConformanceProvider) -> ConformanceFuture {
        let trace = self.trace.clone();
        provider(Box::new(move |storage| {
            Box::pin(async move { replay_case(storage, &trace).await })
        }))
    }
}
/// Backends whose error wording differs; each has its own recorded reference traces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageBackend {
    Memory,
    Jsonl,
    Sqlite,
}
/// The pinned runner-independent storage contracts, executable with the memory backend.
pub fn create_storage_conformance() -> Result<Vec<StorageConformanceCase>> {
    create_storage_conformance_for(StorageBackend::Memory)
}
/// The pinned storage contracts as recorded for `backend`.
pub fn create_storage_conformance_for(
    backend: StorageBackend,
) -> Result<Vec<StorageConformanceCase>> {
    let source = match backend {
        StorageBackend::Memory => include_str!("../../tests/fixtures/storage.json"),
        StorageBackend::Jsonl => include_str!("../../tests/fixtures/storage-jsonl.json"),
        StorageBackend::Sqlite => include_str!("../../tests/fixtures/storage-sqlite.json"),
    };
    let fixture: JsonValue = serde_json::from_str(source)?;
    fixture["cases"]
        .as_array()
        .ok_or_else(|| Error::message("Missing storage conformance cases"))?
        .iter()
        .map(|case| {
            Ok(StorageConformanceCase {
                name: wire::field(case, "name").map_err(Error::message)?,
                trace: case.clone(),
            })
        })
        .collect()
}
