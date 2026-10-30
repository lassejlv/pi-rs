//! Session kernel switches of the embedded `@earendil-works/pi-durable` (`session.*` host
//! methods outside the stock service). The Session itself is the stock service's
//! (`examples/support/session_bridge.rs` through `transport.rs`).
//!
//! - `session.transactionProbes()`: whether this build has pi-durable's `stock-test-probes`, so a
//!   Session can report adoption and discard to `Transaction.prototype` observers
//!   (`js/internal/src/durable/session/transaction.js`).

use crate::host::{Dispatch, Methods};
use crate::value::HostValue;

/// The `session.*` method group.
pub struct SessionDispatch;

impl Dispatch for SessionDispatch {
    fn register(&self, m: &mut Methods) {
        m.sync("session.transactionProbes", |_, _| {
            Ok(HostValue::Bool(cfg!(feature = "stock-test-probes")))
        });
    }
}
