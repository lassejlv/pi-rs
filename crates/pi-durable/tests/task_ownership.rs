use pi_durable::{
    harness::{ownership::*, state::*},
    *,
};

fn task(id: i64, owner: Option<i64>, conversation: i64, background: bool) -> TaskRecord {
    serde_json::from_str(&format!(r#"{{"id":{id},"conversationId":{conversation},"kind":"test","version":1,"input":null,"background":{background},"abortRequested":false,{}"state":{{"status":"pending","checkpoint":{{"phase":"run"}}}}}}"#, owner.map(|owner| format!(r#""owner":{owner},"#)).unwrap_or_default())).unwrap()
}
fn terminal(mut record: TaskRecord) -> TaskRecord {
    record.state = TaskState::Terminal {
        outcome: TaskOutcome::Completed {
            result: JsonValue::Null,
        },
    };
    record
}
fn id(id: i64) -> TaskId {
    TaskId::from_raw(id)
}
fn conversation(id: i64) -> ConversationId {
    ConversationId::from_raw(id)
}
fn overlay(tasks: Vec<TaskRecord>) -> TaskOverlay {
    TaskOverlay::new(tasks, [])
}

#[test]
fn background_tasks_bound_idle_ownership_and_cancellation_scopes() {
    let tree = TaskTree::default();
    tree.set_edge(conversation(1), None);
    let mut owner = task(2, None, 1, false);
    owner.info.abort_requested = true;
    tree.track(owner);
    tree.set_edge(conversation(3), Some(id(2)));
    tree.track(task(4, None, 3, true));
    tree.set_edge(conversation(5), Some(id(4)));
    tree.track(task(6, None, 5, false));
    assert_eq!(tree.owned_live(id(2), None, false, None), []);
    assert_eq!(
        tree.scope_contains(
            Node::Task(id(6)),
            Scope::Conversation(conversation(1)),
            false
        ),
        Some(false)
    );
    assert_eq!(
        tree.scope_contains(
            Node::Task(id(6)),
            Scope::Conversation(conversation(1)),
            true
        ),
        Some(true)
    );
    assert!(tree.cancelling_owner(Node::Task(id(4))).is_none());
    assert_eq!(
        tree.cancelling_owner(Node::Conversation(conversation(3)))
            .unwrap()
            .id(),
        id(2)
    );
    assert!(!tree.idle(Some(conversation(5))));
    tree.track(terminal(task(2, None, 1, false)));
    tree.sweep();
    assert!(tree.idle(None));
    assert_eq!(tree.direct_owned(id(4)).len(), 0); // Conversation ownership is not direct task ownership.
    tree.track(terminal(task(6, None, 5, false)));
    tree.track(terminal(task(4, None, 3, true)));
    tree.sweep();
    let sizes = tree.sizes();
    assert_eq!(
        (
            sizes.live,
            sizes.settled,
            sizes.edges,
            sizes.below,
            sizes.roots,
            sizes.drop_queue
        ),
        (0, 0, 0, 0, 0, 0)
    );
}

#[test]
fn overlays_replace_committed_states_and_connect_new_owned_conversations() {
    let tree = TaskTree::default();
    tree.set_edge(conversation(1), None);
    tree.track(task(2, None, 1, false));
    tree.track(task(3, Some(2), 1, false));
    let mut candidate = overlay(vec![terminal(task(3, Some(2), 1, false))]);
    assert!(
        tree.owned_live(id(2), Some(&candidate), false, None)
            .is_empty()
    );
    candidate.edges.insert(conversation(4), Some(id(2)));
    candidate.set_task(task(5, None, 4, false));
    assert_eq!(
        tree.owned_live(id(2), Some(&candidate), false, None),
        [id(5)]
    );
    assert!(tree.live(id(5)).is_none());
    assert_eq!(tree.owned_live(id(2), None, false, None), [id(3)]);
}

#[tokio::test]
async fn deep_terminal_ancestry_is_iterative_and_reclaimed_after_its_last_live_leaf() {
    let storage = MemoryStorage::new();
    let context = Context::default();
    let depth = 12_000;
    let mut writes = Vec::new();
    let conversation: ConversationRecord = serde_json::from_str(r#"{"id":1}"#).unwrap();
    writes.push(StorageWrite::Conversation {
        value: conversation,
    });
    for n in 2..depth {
        writes.push(StorageWrite::Task {
            value: Box::new(terminal(task(n, (n > 2).then_some(n - 1), 1, false))),
        });
    }
    let leaf = task(depth, Some(depth - 1), 1, false);
    writes.push(StorageWrite::Task {
        value: Box::new(leaf.clone()),
    });
    storage
        .commit(&writes, Box::new(|_| {}), &context)
        .await
        .unwrap();
    let tree = TaskTree::default();
    tree.track(leaf.clone());
    assert!(!tree.idle(None));
    tree.load_chain(Node::Task(leaf.id()), None, &storage, &context)
        .await
        .unwrap();
    tree.sweep();
    assert_eq!(tree.sizes().settled, depth as usize - 2);
    assert_eq!(tree.sizes().unloaded, 0);
    assert!(!tree.idle(None));
    tree.track(terminal(leaf));
    tree.sweep();
    let sizes = tree.sizes();
    assert_eq!(
        (
            sizes.live,
            sizes.settled,
            sizes.edges,
            sizes.below,
            sizes.unloaded
        ),
        (0, 0, 0, 0, 0)
    );
}

#[tokio::test]
async fn wait_for_graph_rejects_cycles_through_explicit_joins_and_ownership() {
    let tree = TaskTree::default();
    tree.set_edge(conversation(1), None);
    let storage = MemoryStorage::new();
    let context = Context::default();
    tree.track(task(2, None, 1, false));
    tree.track(task(3, Some(2), 1, false));
    tree.track(task(4, None, 1, false));
    let empty = overlay(vec![]);
    let error = validate_wait(
        &tree,
        &storage,
        &context,
        InvocationMode::Run,
        id(3),
        &[id(2)],
        JoinPolicy::AllSettled,
        &empty,
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Task 3 cannot wait on 2: 3 -> 2 -> 3 is a wait cycle"
    );
    validate_wait(
        &tree,
        &storage,
        &context,
        InvocationMode::Run,
        id(2),
        &[id(3)],
        JoinPolicy::FailFast,
        &empty,
    )
    .await
    .unwrap();
    assert_eq!(
        validate_wait(
            &tree,
            &storage,
            &context,
            InvocationMode::Run,
            id(2),
            &[id(4)],
            JoinPolicy::FailFast,
            &empty
        )
        .await
        .unwrap_err()
        .to_string(),
        "Task 2 can wait failFast only on tasks it owns; 4 is not one"
    );
    let mut other = task(4, None, 1, false);
    other.state = TaskState::Waiting {
        checkpoint: JsonValue::Null,
        memos: None,
        on: vec![id(3)],
        policy: JoinPolicy::AllSettled,
    };
    tree.track(other.clone());
    assert_eq!(
        validate_wait(
            &tree,
            &storage,
            &context,
            InvocationMode::Run,
            id(3),
            &[id(4)],
            JoinPolicy::AllSettled,
            &empty
        )
        .await
        .unwrap_err()
        .to_string(),
        "Task 3 cannot wait on 4: 3 -> 4 -> 3 is a wait cycle"
    );
    other.info.abort_requested = true;
    tree.track(other);
    validate_wait(
        &tree,
        &storage,
        &context,
        InvocationMode::Run,
        id(3),
        &[id(4)],
        JoinPolicy::AllSettled,
        &empty,
    )
    .await
    .unwrap();
    assert_eq!(
        validate_wait(
            &tree,
            &storage,
            &context,
            InvocationMode::Abort,
            id(2),
            &[],
            JoinPolicy::AllSettled,
            &empty
        )
        .await
        .unwrap_err()
        .to_string(),
        "Abort handler of task 2 cannot wait"
    );
    assert_eq!(
        validate_wait(
            &tree,
            &storage,
            &context,
            InvocationMode::Run,
            id(2),
            &[id(999)],
            JoinPolicy::AllSettled,
            &empty
        )
        .await
        .unwrap_err()
        .to_string(),
        "Task 999 does not exist"
    );
}

#[test]
fn sleep_accepts_fractional_clock_times_and_rejects_invalid_and_abort_times() {
    for valid in [0.0, -0.0, 1.25, f64::MAX] {
        validate_sleep(InvocationMode::Run, id(2), valid).unwrap();
    }
    for (bad, text) in [
        (f64::NAN, "NaN"),
        (f64::INFINITY, "Infinity"),
        (f64::NEG_INFINITY, "-Infinity"),
        (-1.0, "-1"),
    ] {
        assert_eq!(
            validate_sleep(InvocationMode::Run, id(2), bad)
                .unwrap_err()
                .to_string(),
            format!("Task 2 cannot sleep until {text}: not a time in milliseconds")
        );
    }
    assert_eq!(
        validate_sleep(InvocationMode::Abort, id(2), 1.0)
            .unwrap_err()
            .to_string(),
        "Abort handler of task 2 cannot sleep"
    );
}
