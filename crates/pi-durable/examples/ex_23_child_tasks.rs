//! A task that owns child tasks: a checkout charges four payments at once and waits for them.
//! The task graph view prints the checkout's tree while its payments run.
#[path = "ex_b_support/mod.rs"]
mod support;
use parking_lot::Mutex;
use pi_ai::models::Models;
use pi_durable::storage::sqlite::{NativeSqliteStorageOptions, open_native_sqlite_storage};
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    sync::{Arc, LazyLock},
};
use support::*;

// A fake bank: it declines an expired card at once, and confirms other charges after a moment.
static CHARGED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
static LOG: LazyLock<Mutex<Vec<String>>> = LazyLock::new(Mutex::default);
fn log(line: String) {
    println!("{line}");
    LOG.lock().push(line);
}

#[derive(Clone, Serialize, Deserialize)]
struct PaymentInput {
    card: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct PaymentResult {
    card: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "lowercase")]
enum PaymentState {
    Charge,
    Confirm,
}
type PaymentTask = Task<PaymentInput, PaymentState, PaymentResult>;
static PAYMENT: LazyLock<PaymentTask> = LazyLock::new(|| {
    define_task(TaskDefinition {
        name: "example.payment".into(),
        version: 1,
        initial: Arc::new(|_| Ok(PaymentState::Charge)),
        phases: indexmap::IndexMap::from_iter([
            (
                "charge".into(),
                Arc::new(
                    |task: RunningTask<PaymentInput, PaymentState, PaymentResult>,
                     runtime: TaskRuntime<_, _, _, ()>,
                     context| {
                        Box::pin(async move {
                            let card = task.record().info.input.card.clone();
                            if card.starts_with("expired") {
                                let error = failure(&format!("{card} declined"));
                                return runtime
                                    .commit(
                                        move |_, _| async move {
                                            Ok(terminal(TaskOutcome::Failed {
                                                error,
                                                result: None,
                                            }))
                                        },
                                        &context,
                                    )
                                    .await;
                            }
                            CHARGED.lock().insert(card);
                            // The bank confirms after a moment: wait for it without holding an invocation.
                            runtime
                                .sleep_until(
                                    runtime.now()? + 100.0,
                                    PaymentState::Confirm,
                                    &context,
                                )
                                .await
                        }) as storage::StorageFuture<()>
                    },
                ) as PhaseHandler<_, _, _>,
            ),
            (
                "confirm".into(),
                Arc::new(
                    |task: RunningTask<PaymentInput, PaymentState, PaymentResult>,
                     runtime: TaskRuntime<_, _, _, ()>,
                     context| {
                        Box::pin(async move {
                            let card = task.record().info.input.card.clone();
                            runtime
                                .commit(
                                    move |_, _| async move {
                                        Ok(terminal(TaskOutcome::Completed {
                                            result: PaymentResult { card },
                                        }))
                                    },
                                    &context,
                                )
                                .await
                        }) as storage::StorageFuture<()>
                    },
                ) as PhaseHandler<_, _, _>,
            ),
        ]),
        // Each payment undoes its own effect when aborted.
        abort: Arc::new(|task, runtime, context| {
            Box::pin(async move {
                let card = task.record().info.input.card.clone();
                let refunded = CHARGED.lock().remove(&card);
                log(format!(
                    "  payment {card} aborted{}",
                    if refunded { ", refunded" } else { "" }
                ));
                runtime
                    .commit(
                        |_, _| async {
                            Ok(terminal(TaskOutcome::Aborted {
                                reason: None,
                                result: None,
                            }))
                        },
                        &context,
                    )
                    .await
            })
        }),
        migrate: None,
        hooks: None,
    })
});

// The checkout creates its payments as child tasks and waits for all of them.
// With `failFast`, the first payment that fails aborts the others; the checkout
// itself is not aborted and decides its outcome once every payment is done.
#[derive(Clone, Serialize, Deserialize)]
struct CheckoutInput {
    cards: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "lowercase")]
enum CheckoutState {
    Pay,
    Decide {
        payments: Vec<TypedTaskId<PaymentResult>>,
    },
}
type CheckoutTask = Task<CheckoutInput, CheckoutState, String>;
static CHECKOUT: LazyLock<CheckoutTask> = LazyLock::new(|| {
    define_task(TaskDefinition {
        name: "example.checkout".into(),
        version: 1,
        initial: Arc::new(|_| Ok(CheckoutState::Pay)),
        phases: indexmap::IndexMap::from_iter([
            (
                "pay".into(),
                Arc::new(
                    |task: RunningTask<CheckoutInput, CheckoutState, String>,
                     runtime: TaskRuntime<_, _, _, ()>,
                     context| {
                        Box::pin(async move {
                            let id = task.record().id();
                            let cards = task.record().info.input.cards.clone();
                            runtime
                                .commit(
                                    move |tx, _| async move {
                                        let mut payments = Vec::new();
                                        for card in cards {
                                            payments.push(
                                                tx.create_task(
                                                    &PAYMENT,
                                                    PaymentInput { card },
                                                    TaskOptions::new(TaskOwnership::Task(id)),
                                                )
                                                .await?,
                                            );
                                        }
                                        let on = payments
                                            .iter()
                                            .map(|payment| payment.untyped())
                                            .collect();
                                        Ok(Some(NextTaskState::Waiting {
                                            checkpoint: CheckoutState::Decide { payments },
                                            on,
                                            policy: JoinPolicy::FailFast,
                                        }))
                                    },
                                    &context,
                                )
                                .await
                        }) as storage::StorageFuture<()>
                    },
                ) as PhaseHandler<_, _, _>,
            ),
            (
                "decide".into(),
                Arc::new(
                    |task: RunningTask<CheckoutInput, CheckoutState, String>,
                     runtime: TaskRuntime<_, _, _, ()>,
                     context| {
                        Box::pin(async move {
                            let CheckoutState::Decide { payments } = task.checkpoint().clone()
                            else {
                                return Err(Error::message("Checkout is not deciding"));
                            };
                            let outcomes = runtime.outcomes(&payments, &context).await?;
                            log(format!(
                                "  payments: {}",
                                outcomes
                                    .iter()
                                    .map(outcome_word)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ));
                            let paid = outcomes
                                .iter()
                                .all(|outcome| matches!(outcome, TaskOutcome::Completed { .. }));
                            runtime
                                .commit(
                                    move |_, _| async move {
                                        Ok(terminal(if paid {
                                            TaskOutcome::Completed {
                                                result: "order placed".to_string(),
                                            }
                                        } else {
                                            TaskOutcome::Failed {
                                                error: failure("payment failed"),
                                                result: None,
                                            }
                                        }))
                                    },
                                    &context,
                                )
                                .await
                        }) as storage::StorageFuture<()>
                    },
                ) as PhaseHandler<_, _, _>,
            ),
        ]),
        // Runs only after every payment is done, so the refunds have already happened.
        abort: Arc::new(|_, runtime, context| {
            Box::pin(async move {
                log("  checkout aborted".into());
                runtime
                    .commit(
                        |_, _| async {
                            Ok(terminal(TaskOutcome::Aborted {
                                reason: None,
                                result: None,
                            }))
                        },
                        &context,
                    )
                    .await
            })
        }),
        migrate: None,
        hooks: None,
    })
});

/// Print the live tasks as a tree along their owner edges.
fn graph_lines(graph: &TaskGraph) -> Vec<String> {
    fn walk(graph: &TaskGraph, node: &TaskGraphNode, depth: usize, out: &mut Vec<String>) {
        let state = match &node.state {
            TaskGraphState::Waiting { on, .. } => {
                format!(
                    "waiting on {}",
                    on.iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            other => serde_json::to_value(other).unwrap()["status"]
                .as_str()
                .unwrap()
                .to_string(),
        };
        out.push(format!(
            "  {}{} {}: {state}",
            "  ".repeat(depth),
            node.kind,
            node.id
        ));
        for child in graph
            .tasks
            .values()
            .filter(|candidate| candidate.owner == Some(node.id))
        {
            walk(graph, child, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    for node in graph
        .tasks
        .values()
        .filter(|candidate| candidate.owner.is_none())
    {
        walk(graph, node, 0, &mut out);
    }
    out
}

async fn open(registry: &Reg, path: &std::path::Path) -> Result<Harness> {
    let storage = open_native_sqlite_storage(path, NativeSqliteStorageOptions::default()).await?;
    Harness::open(
        Arc::new(storage),
        HarnessOptions::new(registry.clone(), Models::default()),
        &ctx(),
    )
    .await
}
async fn checkout(root: &Conversation, cards: &[&str]) -> Result<TaskId> {
    let cards = cards.iter().map(|card| card.to_string()).collect();
    let id = root
        .commit(
            move |tx| async move {
                tx.create_task(
                    &CHECKOUT,
                    CheckoutInput { cards },
                    TaskOptions::new(TaskOwnership::Conversation),
                )
                .await
            },
            &ctx(),
        )
        .await?;
    Ok(id.untyped())
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    let registry = registry();
    let mut extension = Ext::new("checkout");
    extension.tasks.push(PAYMENT.erased());
    extension.tasks.push(CHECKOUT.erased());
    install(&registry, extension);
    let directory = tempfile::tempdir().map_err(|e| Error::message(e.to_string()))?;
    let path = directory.path().join("session.sqlite");

    let mut harness = open(&registry, &path).await?;
    let mut root = harness.root(Default::default(), &context).await?;

    println!("One card is declined:");
    let mut id = checkout(&root, &["visa-1", "expired-2", "visa-3", "visa-4"]).await?;
    let record = harness.wait_for_task(id, &context).await?;
    println!("  checkout: {}", outcome_status(&record));
    check(
        outcome_status(&record) == "failed",
        "a declined card fails the checkout",
    )?;
    check(
        LOG.lock()
            .iter()
            .any(|line| line.starts_with("  payments:") && line.contains("failed")),
        "the decision saw the failed payment",
    )?;
    check(
        LOG.lock().iter().any(|line| line.contains("aborted")),
        "failFast aborted the other payments",
    )?;

    println!("The customer cancels:");
    id = checkout(&root, &["visa-5", "visa-6", "visa-7", "visa-8"]).await?;
    harness.resume()?;
    pause(20).await;
    let graph = harness.watch_task_graph(&context).await?;
    let lines = graph_lines(&graph.value());
    for line in &lines {
        println!("{line}");
    }
    check(
        lines
            .iter()
            .any(|l| l.contains("example.checkout") && l.contains("waiting on")),
        "checkout waits on payments",
    )?;
    check(
        lines
            .iter()
            .filter(|l| l.contains("example.payment"))
            .count()
            == 4,
        "four payments under the checkout",
    )?;
    graph.stop();
    harness.abort_task(id, &context).await?;
    let record = harness.wait_for_task(id, &context).await?;
    println!("  checkout: {}", outcome_status(&record));
    check(
        outcome_status(&record) == "aborted",
        "cancelled checkout is aborted",
    )?;

    println!("The process stops while the payments run, and a new one continues:");
    id = checkout(&root, &["visa-9", "visa-10", "visa-11", "visa-12"]).await?;
    pause(20).await;
    harness.close(&context).await?;
    harness = open(&registry, &path).await?;
    root = harness.root(Default::default(), &context).await?;
    let record = harness.wait_for_task(id, &context).await?;
    println!("  checkout: {}", outcome_status(&record));
    check(
        outcome_status(&record) == "completed",
        "the restarted process finishes the checkout",
    )?;
    let _ = &root;
    harness.close(&context).await
}
