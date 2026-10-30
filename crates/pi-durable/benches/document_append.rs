//! Appending to a list, one commit per item: one array in one document against one family member per item, as a
//! journal can be stored. Reports milliseconds and microseconds per append at each size. An append costs one copy of
//! the array for the next immutable value; the family's cost stays flat.
//!
//!   cargo bench -p pi-durable --bench document_append          (PI_DURABLE_BENCH_QUICK=1 for small sizes)
mod support;
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Instant};
use support::*;

#[derive(Clone, Serialize, Deserialize)]
struct Item {
    key: String,
    issued: usize,
    at: usize,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Journal {
    entries: Vec<Item>,
}
impl DocumentState for Journal {}
#[derive(Clone, Serialize, Deserialize)]
struct Member {
    issued: usize,
    at: usize,
}
impl DocumentState for Member {}

async fn run(kind: &str, n: usize) -> Result<f64> {
    let journal = define_doc(DocDefinition::new(
        "bench.journal",
        1,
        SessionScope,
        || Ok(Journal::default()),
        |_, _, _| false,
    ))?;
    let journal_lean = define_doc(
        DocDefinition::new(
            "bench.journal-lean",
            1,
            SessionScope,
            || Ok(Journal::default()),
            |_, _, _| false,
        )
        .never_checkpoint(),
    )?;
    let member = define_doc_family(DocFamilyDefinition::new(
        "bench.member",
        1,
        SessionScope,
        |seed: Option<()>| {
            let _ = seed;
            Ok(Member { issued: 0, at: 0 })
        },
        |_, _, _| false,
    ))?;
    let raw = define_doc(DocDefinition::new(
        "bench.raw",
        1,
        SessionScope,
        || Ok(object(r#"{"entries":[]}"#)),
        |_, _, _| false,
    ))?;
    let raw_lean = define_doc(
        DocDefinition::new(
            "bench.raw-lean",
            1,
            SessionScope,
            || Ok(object(r#"{"entries":[]}"#)),
            |_, _, _| false,
        )
        .never_checkpoint(),
    )?;
    let session = Session::new(Arc::new(MemoryStorage::new()), Default::default());
    let context = ctx();
    let started = Instant::now();
    for i in 0..n {
        let (journal, member, raw, journal_lean, raw_lean) = (
            journal.clone(),
            member.clone(),
            raw.clone(),
            journal_lean.clone(),
            raw_lean.clone(),
        );
        let kind = kind.to_string();
        session
            .commit(
                move |tx| async move {
                    if kind == "raw" || kind == "raw-lean" {
                        let raw = if kind == "raw" { &raw } else { &raw_lean };
                        // Explicit operations on the untyped document: no typed round trip.
                        let draft = tx.doc(raw, ()).await?;
                        let len = draft
                            .read()?
                            .get(&JsonString::from("entries"))
                            .and_then(|v| v.as_array().map(Vec::len))
                            .unwrap_or(0);
                        let item = json(&format!(r#"{{"key":"c{i}","issued":{i},"at":{i}}}"#));
                        draft.apply_ops(&[pi_chord::delta::Op::Splice(
                            vec!["entries".into()],
                            len,
                            0,
                            vec![item],
                        )])
                    } else if kind == "array" || kind == "array-lean" {
                        let journal = if kind == "array" {
                            &journal
                        } else {
                            &journal_lean
                        };
                        tx.doc(journal, ()).await?.update(|doc| {
                            doc.entries.push(Item {
                                key: format!("c{i}"),
                                issued: i,
                                at: i,
                            });
                            Ok(())
                        })
                    } else {
                        tx.doc_family(&member, (), format!("c{i}"), None)
                            .await?
                            .update(|doc| {
                                doc.issued = i;
                                doc.at = i;
                                Ok(())
                            })
                    }
                },
                &context,
            )
            .await?;
    }
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    session.close(&context).await?;
    Ok(ms)
}

#[tokio::main]
async fn main() -> Result<()> {
    for n in sizes(&[1000, 8000, 16000, 32000], &[200, 1000]) {
        let only = std::env::var("PI_DURABLE_BENCH_ONLY").ok();
        let go = |kind: &'static str| {
            let skip = only.as_deref().is_some_and(|only| only != kind);
            async move { if skip { Ok(0.0) } else { run(kind, n).await } }
        };
        let array = go("array").await?;
        let raw = go("raw").await?;
        let array_lean = go("array-lean").await?;
        let raw_lean = go("raw-lean").await?;
        let family = go("family").await?;
        let per = |ms: f64| format!("{:.0} us", ms / n as f64 * 1000.0);
        println!(
            "n={n}: array {array:.0} ms ({}/append), raw ops {raw:.0} ms ({}/append), family {family:.0} ms ({}/append); never_checkpoint: array {array_lean:.0} ms ({}/append), raw ops {raw_lean:.0} ms ({}/append)",
            per(array),
            per(raw),
            per(family),
            per(array_lean),
            per(raw_lean)
        );
    }
    Ok(())
}
