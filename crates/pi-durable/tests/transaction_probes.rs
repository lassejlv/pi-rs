#![cfg(feature = "stock-test-probes")]
#[allow(dead_code)]
#[path = "support/session_storage.rs"]
mod support;

use pi_chord::{delta::Op, json::parse};
use pi_durable::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::{ControlledStorage, Fault, Gate};

fn counter() -> SessionDocToken<JsonObject> {
    define_doc(DocDefinition::new(
        "probe.counter",
        1,
        SessionScope,
        || Ok(JsonObject::try_from_value(parse(r#"{"count":0}"#).unwrap()).unwrap()),
        |_, _, _| false,
    ))
    .unwrap()
}
async fn create(session: &Session, doc: SessionDocToken<JsonObject>) {
    session
        .commit(
            move |tx| async move {
                tx.doc(&doc, ()).await?;
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
}
fn bump(session: &Session, doc: SessionDocToken<JsonObject>) -> storage::StorageFuture<()> {
    session.commit(
        move |tx| async move {
            tx.doc(&doc, ())
                .await?
                .apply_ops(&[Op::Set(vec!["count".into()], 1.into())])?;
            Ok(())
        },
        &Context::default(),
    )
}

#[tokio::test]
async fn adoption_failure_preserves_identity_and_never_escapes_into_storage() {
    let storage = Arc::new(ControlledStorage::default());
    let session = Session::new(storage.clone(), SessionOptions::default());
    let doc = counter();
    create(&session, doc.clone()).await;
    let failure = Error::message("adoption failed");
    let injected = failure.clone();
    let discarded = Arc::new(AtomicUsize::new(0));
    let count = discarded.clone();
    session.set_transaction_probe(move |point| match point {
        TransactionProbePoint::Adopt(_) => Err(injected.clone()),
        TransactionProbePoint::Discard => {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });
    let settled = Arc::new(Gate::default());
    *storage.after_adopt.lock() = Some(settled.clone());
    let change = bump(&session, doc.clone());
    settled.entered().await;
    // The backend reached its post-adoption gate, and its durable value is new.
    let context = Context::default();
    let record = storage
        .memory
        .find_document(&doc.address(()), DocumentPoint::Current, &context)
        .await
        .unwrap()
        .unwrap();
    let stored = storage
        .memory
        .document(record.id(), DocumentPoint::Current, &context)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.value.value()["count"].as_u64(), Some(1));
    assert!(session.failure().is_none());
    settled.release();
    assert!(change.await.unwrap_err().same_error(&failure));
    assert_eq!(discarded.load(Ordering::SeqCst), 0);
    let SessionEnd::Failed(error) = session.closed().await else {
        panic!("expected failed Session")
    };
    assert!(error.same_error(&failure));
}

#[tokio::test]
async fn backend_rejection_discards_only_before_adoption() {
    for after in [false, true] {
        let storage = Arc::new(ControlledStorage::default());
        let session = Session::new(storage.clone(), SessionOptions::default());
        let doc = counter();
        create(&session, doc.clone()).await;
        let discarded = Arc::new(AtomicUsize::new(0));
        let count = discarded.clone();
        session.set_transaction_probe(move |point| {
            if matches!(point, TransactionProbePoint::Discard) {
                count.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        });
        let failure = Error::message("backend failed");
        *storage.fault.lock() = Some(if after {
            Fault::After(failure.clone())
        } else {
            Fault::Before(failure.clone())
        });
        assert!(bump(&session, doc).await.unwrap_err().same_error(&failure));
        assert_eq!(discarded.load(Ordering::SeqCst), usize::from(!after));
        assert!(matches!(session.closed().await, SessionEnd::Failed(_)));
    }
}
