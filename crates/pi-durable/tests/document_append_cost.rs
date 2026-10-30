//! Regression tests for the per-commit cost of large documents: a document that never checkpoints must not be
//! decoded after each change, and an untyped (`JsonObject`) document must not round-trip through JSON text.
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

static DECODES: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Default, Serialize)]
struct Counted {
    count: i64,
}
impl<'de> Deserialize<'de> for Counted {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Plain {
            count: i64,
        }
        DECODES.fetch_add(1, Ordering::SeqCst);
        Plain::deserialize(deserializer).map(|plain| Counted { count: plain.count })
    }
}
impl DocumentState for Counted {}

async fn decodes_per_commit(never: bool) -> usize {
    let definition = DocDefinition::new(
        "counted",
        1,
        SessionScope,
        || Ok(Counted::default()),
        |_, _, _| false,
    );
    let token = define_doc(if never {
        definition.never_checkpoint()
    } else {
        definition
    })
    .unwrap();
    let session = Session::new(Arc::new(MemoryStorage::new()), Default::default());
    let context = Context::default();
    let commit = |token: SessionDocToken<Counted>| {
        let session = session.clone();
        let context = context.clone();
        async move {
            session
                .commit(
                    move |tx| async move {
                        tx.doc(&token, ()).await?.update(|state| {
                            state.count += 1;
                            Ok(())
                        })
                    },
                    &context,
                )
                .await
                .unwrap();
        }
    };
    // The first commit creates the document; measure the second.
    commit(token.clone()).await;
    let before = DECODES.load(Ordering::SeqCst);
    commit(token.clone()).await;
    let used = DECODES.load(Ordering::SeqCst) - before;
    session.close(&context).await.unwrap();
    used
}

#[tokio::test]
async fn a_document_that_never_checkpoints_is_not_decoded_after_each_change() {
    let with_predicate = decodes_per_commit(false).await;
    let without = decodes_per_commit(true).await;
    assert_eq!(
        with_predicate,
        without + 1,
        "the checkpoint predicate costs one full decode per commit"
    );
}

#[tokio::test]
async fn untyped_documents_round_trip_raw_utf16_without_a_text_pass() {
    let token = define_doc(DocDefinition::new(
        "raw",
        1,
        SessionScope,
        || Ok(JsonObject::default()),
        |_, _, _| false,
    ))
    .unwrap();
    let session = Session::new(Arc::new(MemoryStorage::new()), Default::default());
    let context = Context::default();
    let lone = JsonString::from_utf16(vec![0x61, 0xd800, 0x62]);
    let expected = lone.clone();
    let key = lone.clone();
    session
        .commit(
            move |tx| async move {
                tx.doc(&token, ()).await?.update(|object| {
                    object.insert(key, JsonValue::String(lone));
                    Ok(())
                })
            },
            &context,
        )
        .await
        .unwrap();
    let token = define_doc(DocDefinition::new(
        "raw",
        1,
        SessionScope,
        || Ok(JsonObject::default()),
        |_, _, _| false,
    ))
    .unwrap();
    let stored = session
        .snapshot(&token, (), &context)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.get(&expected),
        Some(&JsonValue::String(expected.clone()))
    );
    session.close(&context).await.unwrap();
}

#[test]
fn integral_numbers_print_like_ecmascript() {
    for (value, text) in [
        (0.0, "0"),
        (-0.0, "0"),
        (7.0, "7"),
        (-5.0, "-5"),
        (1e15, "1000000000000000"),
        (9007199254740991.0, "9007199254740991"),
        (9007199254740992.0, "9007199254740992"),
        (1e21, "1e+21"),
        (123456789012345680000.0, "123456789012345680000"),
        (1.5, "1.5"),
        (-0.000001, "-0.000001"),
    ] {
        assert_eq!(pi_json::number_to_string(value), text, "{value}");
    }
}

#[test]
fn the_live_document_decides_checkpoints_from_its_stored_json() {
    use pi_durable::harness::live::LIVE_DOC;
    let definition = LIVE_DOC.definition().clone();
    let decide = |text: &str| {
        let value = JsonObject::try_from_value(pi_chord::json::parse(text).unwrap()).unwrap();
        definition
            .checkpoint_when(
                &value,
                &[],
                CheckpointInfo {
                    deltas_since_base: 0,
                },
            )
            .unwrap()
    };
    let slot =
        |status: &str| format!(r#"{{"callId":"c","name":"n","status":"{status}","taskId":1}}"#);
    assert!(decide("{}"), "idle");
    assert!(
        decide(&format!(r#"{{"tools":[{}]}}"#, slot("done"))),
        "finished tools"
    );
    assert!(
        !decide(&format!(r#"{{"tools":[{}]}}"#, slot("running"))),
        "a running tool"
    );
    assert!(
        !decide(&format!(
            r#"{{"nestedTools":[{},{}]}}"#,
            slot("done"),
            slot("running")
        )),
        "a running nested tool"
    );
    assert!(
        !decide(r#"{"generation":{"attempt":1}}"#),
        "a generation in flight"
    );
}
