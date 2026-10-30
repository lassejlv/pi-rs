//! Host method groups. Each `src/dispatch/<group>.rs` implements [`crate::Dispatch`] and is
//! listed in [`defaults`].

use std::sync::Arc;

use crate::host::Dispatch;

pub mod chord;
pub mod codemode;
pub mod env;
pub mod indexeddb;
pub mod harness;
pub mod images;
pub mod session;
pub mod storage;
pub mod telemetry;
pub mod testing;
pub mod tools;
pub mod transport;
pub mod vitest;

/// The method groups installed by default.
pub fn defaults() -> Vec<Arc<dyn Dispatch>> {
    vec![
        Arc::new(chord::ChordDispatch),
        Arc::new(codemode::CodemodeDispatch),
        Arc::new(env::EnvDispatch),
        Arc::new(tools::ToolsDispatch),
        Arc::new(images::ImagesDispatch),
        Arc::new(testing::TestingDispatch),
        Arc::new(telemetry::TelemetryDispatch),
        Arc::new(session::SessionDispatch),
        Arc::new(storage::StorageDispatch),
        Arc::new(indexeddb::IndexedDbDispatch),
        Arc::new(transport::TransportDispatch),
        Arc::new(harness::HarnessDispatch),
    ]
}
