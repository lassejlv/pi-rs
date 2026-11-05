//! Run a durable task: a state machine whose checkpoint is saved after every step.
//! Port of upstream test/examples/11-tasks.ts.
#[path = "ex_support/mod.rs"]
mod support;
use parking_lot::Mutex;
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use support::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PaymentInput {
    amount: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "lowercase")]
enum PaymentState {
    Prepare,
    Charge { key: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Receipt {
    receipt: u64,
}
type PaymentRuntime = TaskRuntime<PaymentInput, PaymentState, Receipt, ()>;

#[tokio::main]
async fn main() -> Result<()> {
    let context = Context::default();
    // The fake payment service ignores a repeated key, so a rerun of the charge step is safe.
    let payments: Arc<Mutex<HashMap<String, u64>>> = Arc::default();

    // One handler per phase. Each saves progress through runtime.commit(): its callback returns
    // the next checkpoint or the final outcome, saved in the same commit as everything else the
    // callback wrote.
    let prepare: PhaseHandler<PaymentInput, PaymentState, Receipt, ()> =
        Arc::new(|task, runtime: PaymentRuntime, task_context| {
            Box::pin(async move {
                let key = format!("payment-{}", task.record().id());
                runtime
                    .commit(
                        move |_, _| async move {
                            Ok(Some(NextTaskState::Running {
                                checkpoint: PaymentState::Charge { key },
                            }))
                        },
                        &task_context,
                    )
                    .await
            })
        });
    let ledger = payments.clone();
    let charge: PhaseHandler<PaymentInput, PaymentState, Receipt, ()> =
        Arc::new(move |task, runtime: PaymentRuntime, task_context| {
            let ledger = ledger.clone();
            Box::pin(async move {
                let PaymentState::Charge { key } = task.checkpoint().clone() else {
                    return Err(Error::message("charge phase without a key"));
                };
                let amount = task.record().info.input.amount;
                let receipt = *ledger.lock().entry(key).or_insert(amount * 100);
                runtime
                    .commit(
                        move |_, _| async move {
                            Ok(Some(NextTaskState::Terminal {
                                outcome: TaskOutcome::Completed {
                                    result: Receipt { receipt },
                                },
                            }))
                        },
                        &task_context,
                    )
                    .await
            })
        });
    // Runs instead of the phases after an abort; it decides the outcome.
    let abort: PhaseHandler<PaymentInput, PaymentState, Receipt, ()> =
        Arc::new(|_, runtime: PaymentRuntime, task_context| {
            Box::pin(async move {
                runtime
                    .commit(
                        |_, _| async {
                            Ok(Some(NextTaskState::Terminal {
                                outcome: TaskOutcome::Aborted {
                                    reason: None,
                                    result: None,
                                },
                            }))
                        },
                        &task_context,
                    )
                    .await
            })
        });
    let payment = define_task(TaskDefinition {
        name: "example.payment".into(),
        version: 1,
        initial: Arc::new(|_| Ok(PaymentState::Prepare)),
        phases: [("prepare".into(), prepare), ("charge".into(), charge)]
            .into_iter()
            .collect(),
        abort,
        migrate: None,
        hooks: None,
    });

    // Extensions bring task code; the Harness finds it by name in the registry. Nothing runs
    // until resume() or a call that waits for progress, such as wait_for_task().
    let registry = create_registry::<NativeTool, NativeSection>()?;
    let mut payments_extension = define_extension(Ext::new("payments"));
    payments_extension.tasks.push(payment.erased());
    registry.install(Arc::new(payments_extension))?;
    let harness = Harness::open(
        Arc::new(MemoryStorage::new()),
        HarnessOptions::new(registry, pi_ai::models::Models::default()),
        &context,
    )
    .await?;
    let root = harness
        .root(ConversationOptions::default(), &context)
        .await?;

    // Every task names its owner. This one belongs to the conversation.
    let task = payment.clone();
    let payment_id = root
        .commit(
            move |tx| async move {
                tx.create_task(
                    &task,
                    PaymentInput { amount: 5 },
                    TaskOptions::new(TaskOwnership::Conversation),
                )
                .await
            },
            &context,
        )
        .await?;
    // The finished task record is the durable receipt.
    let paid = harness
        .wait_for_task(payment_id.untyped(), &context)
        .await?;
    println!("payment outcome: {:?}", paid.state);
    let TaskState::Terminal { outcome } = paid.into_record().state else {
        return Err(Error::message("payment did not finish"));
    };
    let outcome = serde_json::to_value(&outcome).expect("outcome serializes");
    assert_eq!(
        outcome,
        serde_json::json!({"status": "completed", "result": {"receipt": 500}})
    );
    assert_eq!(payments.lock().len(), 1);

    harness.close(&context).await
}
