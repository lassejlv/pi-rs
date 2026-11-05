//! State transitions and wait-for validation shared by task invocations and the scheduler.
use super::ownership::{Node, TaskOverlay, TaskTree};
use crate::*;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvocationMode {
    Run,
    Abort,
}

/// States a task may commit for itself. Pending reservations and held outcomes
/// belong to the scheduler rather than task code.
#[derive(Clone, Debug, PartialEq)]
pub enum NextTaskState<S = JsonValue, R = JsonValue> {
    Running {
        checkpoint: S,
    },
    Waiting {
        checkpoint: S,
        on: Vec<TaskId>,
        policy: JoinPolicy,
    },
    Sleeping {
        checkpoint: S,
        until: f64,
    },
    Terminal {
        outcome: TaskOutcome<R>,
    },
}
impl<S, R> NextTaskState<S, R> {
    pub fn into_state(self) -> TaskState<S, R> {
        match self {
            Self::Running { checkpoint } => TaskState::Running {
                checkpoint,
                memos: None,
            },
            Self::Waiting {
                checkpoint,
                on,
                policy,
            } => TaskState::Waiting {
                checkpoint,
                on,
                policy,
                memos: None,
            },
            Self::Sleeping { checkpoint, until } => TaskState::Sleeping {
                checkpoint,
                until,
                memos: None,
            },
            Self::Terminal { outcome } => TaskState::Terminal { outcome },
        }
    }
}

pub fn with_state(mut record: TaskRecord, mut state: TaskState) -> TaskRecord {
    match &mut state {
        TaskState::Completing { .. } | TaskState::Terminal { .. } => record.info.restarts = None,
        TaskState::Pending { memos, .. }
        | TaskState::Running { memos, .. }
        | TaskState::Waiting { memos, .. }
        | TaskState::Sleeping { memos, .. } => {
            *memos = record.state.memos().cloned();
        }
    }
    record.state = state;
    record
}

pub fn validate_sleep(mode: InvocationMode, task: TaskId, until: f64) -> Result<()> {
    if mode == InvocationMode::Abort {
        return Err(Error::message(format!(
            "Abort handler of task {task} cannot sleep"
        )));
    }
    if !until.is_finite() || until < 0.0 {
        let value = if until.is_nan() {
            "NaN".into()
        } else if until == f64::INFINITY {
            "Infinity".into()
        } else if until == f64::NEG_INFINITY {
            "-Infinity".into()
        } else {
            until.to_string()
        };
        return Err(Error::message(format!(
            "Task {task} cannot sleep until {value}: not a time in milliseconds"
        )));
    }
    Ok(())
}

pub async fn load_scopes(tree: &TaskTree, storage: &dyn Storage, context: &Context) -> Result<()> {
    for id in tree.unloaded() {
        tree.load_chain(Node::Task(id), None, storage, context)
            .await?;
    }
    Ok(())
}

/// A task waits for explicit joins and all live ordinary owned work. Only a new
/// wait adds an edge between existing tasks, so checking it preserves acyclicity.
#[allow(clippy::too_many_arguments)]
pub async fn validate_wait(
    tree: &TaskTree,
    storage: &dyn Storage,
    context: &Context,
    mode: InvocationMode,
    current: TaskId,
    on: &[TaskId],
    policy: JoinPolicy,
    overlay: &TaskOverlay,
) -> Result<()> {
    if mode == InvocationMode::Abort {
        return Err(Error::message(format!(
            "Abort handler of task {current} cannot wait"
        )));
    }
    let mut targets = Vec::new();
    for id in on {
        let member = if let Some(record) = overlay.tasks.get(id).cloned().or_else(|| tree.live(*id))
        {
            (*record).clone()
        } else {
            storage
                .task(*id, context)
                .await?
                .ok_or_else(|| Error::message(format!("Task {id} does not exist")))?
        };
        if policy == JoinPolicy::FailFast && member.info.owner != Some(current) {
            return Err(Error::message(format!(
                "Task {current} can wait failFast only on tasks it owns; {id} is not one"
            )));
        }
        if member.state.status() != TaskStatus::Terminal && member.info.owner != Some(current) {
            targets.push(*id);
        }
    }
    if targets.is_empty() {
        return Ok(());
    }
    load_scopes(tree, storage, context).await?;
    if let Some(cycle) = wait_cycle(tree, current, &targets, overlay) {
        let path = std::iter::once(current)
            .chain(cycle.iter().copied())
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(" -> ");
        return Err(Error::message(format!(
            "Task {current} cannot wait on {}: {path} is a wait cycle",
            cycle[0]
        )));
    }
    Ok(())
}

fn wait_cycle(
    tree: &TaskTree,
    task: TaskId,
    on: &[TaskId],
    overlay: &TaskOverlay,
) -> Option<Vec<TaskId>> {
    let mut from = HashMap::<TaskId, Option<TaskId>>::new();
    let mut walked = HashSet::new();
    let mut pending = Vec::new();
    fn reach(
        id: TaskId,
        via: Option<TaskId>,
        from: &mut HashMap<TaskId, Option<TaskId>>,
        pending: &mut Vec<TaskId>,
    ) {
        if let std::collections::hash_map::Entry::Vacant(entry) = from.entry(id) {
            entry.insert(via);
            pending.push(id);
        }
    }
    for id in on {
        reach(*id, None, &mut from, &mut pending);
    }
    while let Some(id) = pending.pop() {
        if id == task {
            let mut path = Vec::new();
            let mut at = Some(id);
            while let Some(id) = at {
                path.push(id);
                at = from[&id];
            }
            path.reverse();
            return Some(path);
        }
        let Some(record) = overlay.tasks.get(&id).cloned().or_else(|| tree.live(id)) else {
            continue;
        };
        if record.state.status() == TaskStatus::Terminal {
            continue;
        }
        if let TaskState::Waiting { on, .. } = &record.state
            && !record.info.abort_requested
        {
            for member in on {
                reach(*member, Some(id), &mut from, &mut pending);
            }
        }
        if !walked.insert(id) {
            continue;
        }
        let found = tree.walk_down(Node::Task(id), Some(overlay), false, |below| {
            !walked.contains(&below)
        });
        for below in found
            .iter()
            .filter(|r| r.live.is_some() && !r.node.background)
        {
            reach(below.id, Some(id), &mut from, &mut pending);
        }
        walked.extend(
            found
                .into_iter()
                .filter(|r| r.live.is_some() && !r.node.background)
                .map(|r| r.id),
        );
    }
    None
}
