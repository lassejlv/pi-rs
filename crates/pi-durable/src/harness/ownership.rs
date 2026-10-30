//! The scheduler's ownership tree, bounded by live work. All mutations are made
//! by its Session-line callbacks; storage reads never hold this index's mutex.
use super::scheduler::ordered::OrderedSet;
use crate::*;
use indexmap::{IndexMap, IndexSet};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Node {
    Task(TaskId),
    Conversation(ConversationId),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskNode {
    pub conversation_id: ConversationId,
    pub owner: Option<TaskId>,
    pub background: bool,
}
impl TaskNode {
    pub fn of(record: &TaskRecord) -> Self {
        Self {
            conversation_id: record.info.conversation_id,
            owner: record.info.owner,
            background: record.info.background,
        }
    }
    pub fn parent(self) -> Node {
        self.owner
            .map_or(Node::Conversation(self.conversation_id), Node::Task)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Conversation(ConversationId),
    Roots,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ancestor {
    Task(TaskId, TaskNode),
    Conversation(ConversationId),
    Unknown,
}
#[derive(Clone, Debug)]
pub struct Reached {
    pub id: TaskId,
    pub node: TaskNode,
    pub live: Option<Arc<TaskRecord>>,
}
type Links = IndexMap<Node, IndexSet<Node>>;
pub struct TaskOverlay {
    pub tasks: IndexMap<TaskId, Arc<TaskRecord>>,
    pub edges: HashMap<ConversationId, Option<TaskId>>,
    links: OnceLock<Links>,
}
impl TaskOverlay {
    pub fn new(
        tasks: impl IntoIterator<Item = TaskRecord>,
        conversations: impl IntoIterator<Item = ConversationRecord>,
    ) -> Self {
        Self {
            tasks: tasks.into_iter().map(|t| (t.id(), Arc::new(t))).collect(),
            edges: conversations
                .into_iter()
                .map(|c| (c.id, c.owner.map(|o| o.task_id)))
                .collect(),
            links: OnceLock::new(),
        }
    }
    pub fn from_transaction(tx: &Transaction) -> Self {
        Self::new(tx.staged_tasks(), tx.staged_conversations())
    }
    pub fn set_task(&mut self, record: TaskRecord) {
        let kept = self
            .tasks
            .get(&record.id())
            .is_some_and(|staged| TaskNode::of(staged) == TaskNode::of(&record));
        self.tasks.insert(record.id(), Arc::new(record));
        if !kept {
            self.links.take();
        }
    }
}
#[derive(Default)]
struct State {
    live: IndexMap<TaskId, Arc<TaskRecord>>,
    intent: IndexSet<TaskId>,
    settled: HashMap<TaskId, TaskNode>,
    edges: HashMap<ConversationId, Option<TaskId>>,
    below: Links,
    roots: IndexSet<ConversationId>,
    unloaded: OrderedSet<TaskId>,
    drop_queue: IndexSet<Node>,
}
#[derive(Clone, Default)]
pub struct TaskTree {
    state: Arc<Mutex<State>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexSizes {
    pub live: usize,
    pub settled: usize,
    pub edges: usize,
    pub below: usize,
    pub roots: usize,
    pub unloaded: usize,
    pub drop_queue: usize,
}
pub fn failed_outcome(record: &TaskRecord) -> bool {
    matches!(&record.state,TaskState::Completing {outcome}|TaskState::Terminal {outcome} if !matches!(outcome,TaskOutcome::Completed {..}))
}
pub fn cancellation_intent(record: &TaskRecord) -> bool {
    record.state.status() != TaskStatus::Terminal
        && (record.info.abort_requested || failed_outcome(record))
}
impl TaskTree {
    pub fn live(&self, id: TaskId) -> Option<Arc<TaskRecord>> {
        self.state.lock().live.get(&id).cloned()
    }
    pub fn live_records(&self) -> Vec<Arc<TaskRecord>> {
        self.state.lock().live.values().cloned().collect()
    }
    pub fn unloaded(&self) -> Vec<TaskId> {
        self.state.lock().unloaded.iter().copied().collect()
    }
    pub fn set_edge(&self, id: ConversationId, owner: Option<TaskId>) {
        self.state.lock().set_edge(id, owner);
    }
    /// Stage a new state of a task in `overlay`. Overlay links only connect tasks this tree
    /// does not hold live, so a live task with unchanged ownership fields keeps the cached
    /// links; recomputing them for each of a long chain's finalizations is quadratic.
    pub fn stage(&self, overlay: &mut TaskOverlay, record: TaskRecord) {
        let live = self
            .state
            .lock()
            .live
            .get(&record.id())
            .is_some_and(|live| TaskNode::of(live) == TaskNode::of(&record));
        if live {
            overlay.tasks.insert(record.id(), Arc::new(record));
        } else {
            overlay.set_task(record);
        }
    }
    /// Track one complete committed record. Returns the live owner potentially
    /// freed by terminalization, for finalization and bottom-up abort scheduling.
    pub fn track(&self, record: TaskRecord) -> Option<Arc<TaskRecord>> {
        let mut state = self.state.lock();
        let id = record.id();
        if record.state.status() == TaskStatus::Terminal {
            let node = TaskNode::of(&record);
            state.live.shift_remove(&id);
            state.intent.shift_remove(&id);
            state.settled.insert(id, node);
            let above = state.nearest_live_above(node, None);
            state.relist(Node::Task(id));
            state.drop_if_unneeded(Node::Task(id));
            above
        } else {
            if cancellation_intent(&record) {
                state.intent.insert(id);
            } else {
                state.intent.shift_remove(&id);
            }
            let first = !state.live.contains_key(&id);
            let record = Arc::new(record);
            state.live.insert(id, record.clone());
            if first {
                if !state.known(TaskNode::of(&record).parent()) {
                    state.unloaded.insert(id);
                }
                state.relist(Node::Task(id));
            }
            None
        }
    }
    pub fn above(&self, start: Node, overlay: Option<&TaskOverlay>) -> Vec<Ancestor> {
        self.state.lock().above(start, overlay)
    }
    pub fn known(&self, node: Node) -> bool {
        self.state.lock().known(node)
    }
    pub fn nearest_live_above(
        &self,
        node: TaskNode,
        overlay: Option<&TaskOverlay>,
    ) -> Option<Arc<TaskRecord>> {
        self.state.lock().nearest_live_above(node, overlay)
    }
    pub fn walk_down(
        &self,
        start: Node,
        overlay: Option<&TaskOverlay>,
        cross_background: bool,
        enter: impl Fn(TaskId) -> bool,
    ) -> Vec<Reached> {
        self.state
            .lock()
            .walk_down(start, overlay, cross_background, &enter, false)
    }
    pub fn owned_live(
        &self,
        owner: TaskId,
        overlay: Option<&TaskOverlay>,
        first: bool,
        done: Option<&HashSet<TaskId>>,
    ) -> Vec<TaskId> {
        let found = self.state.lock().walk_down(
            Node::Task(owner),
            overlay,
            false,
            &|id| !done.is_some_and(|done| done.contains(&id)),
            first,
        );
        let ids = found
            .into_iter()
            .filter(|r| r.live.is_some() && !r.node.background)
            .map(|r| r.id);
        if first {
            ids.take(1).collect()
        } else {
            ids.collect()
        }
    }
    pub fn scope_contains(
        &self,
        start: Node,
        scope: Scope,
        cross_background: bool,
    ) -> Option<bool> {
        for step in self.above(start, None) {
            match step {
                Ancestor::Unknown => return None,
                Ancestor::Conversation(id) if scope == Scope::Conversation(id) => {
                    return Some(true);
                }
                Ancestor::Task(_, node) if node.background && !cross_background => {
                    return Some(false);
                }
                _ => {}
            }
        }
        Some(scope == Scope::Roots)
    }
    pub fn cancelling_owner(&self, start: Node) -> Option<Arc<TaskRecord>> {
        let state = self.state.lock();
        if state.intent.is_empty() {
            return None;
        }
        for step in state.above(start, None) {
            match step {
                Ancestor::Unknown => return None,
                Ancestor::Task(id, node) => {
                    if let Some(live) = state.live.get(&id).filter(|r| cancellation_intent(r)) {
                        return Some(live.clone());
                    }
                    if node.background {
                        return None;
                    }
                }
                _ => {}
            }
        }
        None
    }
    pub fn cancellation_intents(&self) -> Vec<Arc<TaskRecord>> {
        let state = self.state.lock();
        state
            .intent
            .iter()
            .filter_map(|id| state.live.get(id).cloned())
            .collect()
    }
    pub fn has_cancellation_intents(&self) -> bool {
        !self.state.lock().intent.is_empty()
    }
    pub fn idle(&self, conversation: Option<ConversationId>) -> bool {
        let unknown = {
            let state = self.state.lock();
            state
                .unloaded
                .iter()
                .filter_map(|id| state.live.get(id))
                .cloned()
                .collect::<Vec<_>>()
        };
        let scope = conversation.map_or(Scope::Roots, Scope::Conversation);
        for record in unknown {
            if !record.info.background
                && self.scope_contains(TaskNode::of(&record).parent(), scope, false) != Some(false)
            {
                return false;
            }
        }
        let state = self.state.lock();
        let starts = conversation.map_or_else(
            || state.roots.iter().copied().collect::<Vec<_>>(),
            |id| vec![id],
        );
        for start in starts {
            if state
                .walk_down(Node::Conversation(start), None, false, &|_| true, true)
                .iter()
                .any(|r| r.live.is_some() && !r.node.background)
            {
                return false;
            }
        }
        true
    }
    pub fn direct_owned(&self, owner: TaskId) -> Vec<Arc<TaskRecord>> {
        let state = self.state.lock();
        let mut result = state
            .below
            .get(&Node::Task(owner))
            .into_iter()
            .flatten()
            .filter_map(|n| match n {
                Node::Task(id) => state
                    .live
                    .get(id)
                    .filter(|r| r.info.owner == Some(owner))
                    .cloned(),
                _ => None,
            })
            .collect::<Vec<_>>();
        result.sort_by_key(|r| r.id());
        result
    }
    pub fn sweep(&self) {
        let mut state = self.state.lock();
        let drops = std::mem::take(&mut state.drop_queue);
        for node in drops {
            if state.below.contains_key(&node) {
                continue;
            }
            match node {
                Node::Conversation(id) => {
                    state.edges.remove(&id);
                    state.roots.shift_remove(&id);
                }
                Node::Task(id) if !state.live.contains_key(&id) => {
                    state.settled.remove(&id);
                    state.unloaded.shift_remove(&id);
                }
                _ => {}
            }
        }
    }
    pub fn clear(&self) {
        *self.state.lock() = State::default();
    }
    pub fn has_drops(&self) -> bool {
        !self.state.lock().drop_queue.is_empty()
    }
    pub fn sizes(&self) -> IndexSizes {
        let s = self.state.lock();
        IndexSizes {
            live: s.live.len(),
            settled: s.settled.len(),
            edges: s.edges.len(),
            below: s.below.values().map(IndexSet::len).sum(),
            roots: s.roots.len(),
            unloaded: s.unloaded.len(),
            drop_queue: s.drop_queue.len(),
        }
    }
    /// Load only missing immutable ownership fields, without retaining terminal
    /// payloads. New task candidates are covered by their own parent-chain load.
    pub async fn load_chain(
        &self,
        start: Node,
        overlay: Option<&TaskOverlay>,
        storage: &dyn Storage,
        context: &Context,
    ) -> Result<()> {
        let mut passed = Vec::new();
        let mut at = Some(start);
        let mut visited = HashSet::new();
        while let Some(node) = at {
            if !visited.insert(node) {
                return Err(Error::message(format!(
                    "Task ownership contains a cycle at {node:?}"
                )));
            }
            match node {
                Node::Task(id) => {
                    let known = {
                        let state = self.state.lock();
                        state
                            .live
                            .get(&id)
                            .map(|r| TaskNode::of(r))
                            .or_else(|| state.settled.get(&id).copied())
                            .map(|n| (n, !state.unloaded.contains(&id)))
                    };
                    let fields = if let Some((fields, loaded)) = known {
                        if loaded {
                            break;
                        }
                        fields
                    } else {
                        if overlay.is_some_and(|o| o.tasks.contains_key(&id)) {
                            break;
                        }
                        let Some(record) = storage.task(id, context).await? else {
                            return Ok(());
                        };
                        let fields = TaskNode::of(&record);
                        if record.state.status() == TaskStatus::Terminal {
                            let mut state = self.state.lock();
                            state.settled.insert(id, fields);
                            state.unloaded.insert(id);
                            state.relist(node);
                            state.drop_if_unneeded(node);
                        }
                        fields
                    };
                    passed.push(id);
                    at = Some(fields.parent());
                }
                Node::Conversation(id) => {
                    let edge = { self.state.lock().edge(id, overlay) };
                    let owner = if let Some(edge) = edge {
                        edge
                    } else {
                        let record = storage.conversation(id, context).await?;
                        let owner = record.and_then(|r| r.owner.map(|o| o.task_id));
                        self.set_edge(id, owner);
                        owner
                    };
                    at = owner.map(Node::Task);
                }
            }
        }
        let mut state = self.state.lock();
        for id in passed {
            state.unloaded.shift_remove(&id);
        }
        Ok(())
    }
}
impl State {
    fn edge(&self, id: ConversationId, overlay: Option<&TaskOverlay>) -> Option<Option<TaskId>> {
        overlay
            .and_then(|o| o.edges.get(&id).copied())
            .or_else(|| self.edges.get(&id).copied())
    }
    fn task_node(&self, id: TaskId, overlay: Option<&TaskOverlay>) -> Option<TaskNode> {
        overlay
            .and_then(|o| o.tasks.get(&id))
            .or_else(|| self.live.get(&id))
            .map(|r| TaskNode::of(r))
            .or_else(|| self.settled.get(&id).copied())
    }
    fn known(&self, node: Node) -> bool {
        match node {
            Node::Task(id) => {
                (self.live.contains_key(&id) || self.settled.contains_key(&id))
                    && !self.unloaded.contains(&id)
            }
            Node::Conversation(id) => self
                .edges
                .get(&id)
                .is_some_and(|owner| owner.is_none_or(|id| self.known(Node::Task(id)))),
        }
    }
    fn set_edge(&mut self, id: ConversationId, owner: Option<TaskId>) {
        self.edges.insert(id, owner);
        self.relist(Node::Conversation(id));
        self.drop_if_unneeded(Node::Conversation(id));
    }
    fn drop_if_unneeded(&mut self, node: Node) {
        if matches!(node,Node::Task(id) if self.live.contains_key(&id))
            || self.below.contains_key(&node)
        {
            return;
        }
        self.drop_queue.insert(node);
    }
    fn relist(&mut self, start: Node) {
        let mut node = start;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(node) {
                return;
            }
            let needed = matches!(node,Node::Task(id) if self.live.contains_key(&id))
                || self.below.contains_key(&node);
            let parent = match node {
                Node::Conversation(id) => match self.edges.get(&id) {
                    None => return,
                    Some(None) => {
                        if needed {
                            self.roots.insert(id);
                        } else {
                            self.roots.shift_remove(&id);
                        }
                        return;
                    }
                    Some(Some(owner)) => Node::Task(*owner),
                },
                Node::Task(id) => match self.task_node(id, None) {
                    Some(n) => n.parent(),
                    None => return,
                },
            };
            if self.below.get(&parent).is_some_and(|s| s.contains(&node)) == needed {
                return;
            }
            if needed {
                let first = !self.below.contains_key(&parent);
                self.below.entry(parent).or_default().insert(node);
                if !first {
                    return;
                }
            } else {
                if let Some(children) = self.below.get_mut(&parent) {
                    children.shift_remove(&node);
                    if !children.is_empty() {
                        return;
                    }
                }
                self.below.shift_remove(&parent);
                self.drop_if_unneeded(parent);
            }
            node = parent;
        }
    }
    fn nearest_live_above(
        &self,
        mut node: TaskNode,
        overlay: Option<&TaskOverlay>,
    ) -> Option<Arc<TaskRecord>> {
        let mut visited = HashSet::new();
        while !node.background {
            let owner = node
                .owner
                .or_else(|| self.edge(node.conversation_id, overlay).flatten())?;
            if !visited.insert(owner) {
                return None;
            }
            let record = overlay
                .and_then(|o| o.tasks.get(&owner))
                .or_else(|| self.live.get(&owner));
            if let Some(record) = record.filter(|r| r.state.status() != TaskStatus::Terminal) {
                return Some(record.clone());
            }
            node = record
                .map(|r| TaskNode::of(r))
                .or_else(|| self.settled.get(&owner).copied())?;
        }
        None
    }
    fn above(&self, start: Node, overlay: Option<&TaskOverlay>) -> Vec<Ancestor> {
        let mut result = Vec::new();
        let mut at = Some(start);
        let mut visited = HashSet::new();
        while let Some(node) = at {
            if !visited.insert(node) {
                result.push(Ancestor::Unknown);
                break;
            }
            at = match node {
                Node::Task(id) => {
                    let Some(fields) = self.task_node(id, overlay) else {
                        result.push(Ancestor::Unknown);
                        break;
                    };
                    result.push(Ancestor::Task(id, fields));
                    Some(fields.parent())
                }
                Node::Conversation(id) => {
                    result.push(Ancestor::Conversation(id));
                    let Some(edge) = self.edge(id, overlay) else {
                        result.push(Ancestor::Unknown);
                        break;
                    };
                    edge.map(Node::Task)
                }
            };
        }
        result
    }
    fn overlay_links(&self, overlay: &TaskOverlay) -> Links {
        let mut links = Links::new();
        for record in overlay.tasks.values() {
            if self.live.contains_key(&record.id()) || self.settled.contains_key(&record.id()) {
                continue;
            }
            let mut child = Node::Task(record.id());
            let mut parent = Some(TaskNode::of(record).parent());
            let mut visited = HashSet::new();
            while let Some(node) = parent {
                if !visited.insert(node) || links.get(&node).is_some_and(|s| s.contains(&child)) {
                    break;
                }
                links.entry(node).or_default().insert(child);
                if matches!(node,Node::Task(id) if self.live.contains_key(&id)||overlay.tasks.contains_key(&id))
                {
                    break;
                }
                let above = match node {
                    Node::Conversation(id) => {
                        self.edge(id, Some(overlay)).flatten().map(Node::Task)
                    }
                    Node::Task(id) => self.settled.get(&id).map(|n| n.parent()),
                };
                if above.is_some_and(|above| {
                    self.below
                        .get(&above)
                        .is_some_and(|children| children.contains(&node))
                }) {
                    break;
                }
                child = node;
                parent = above;
            }
        }
        links
    }
    fn walk_down(
        &self,
        start: Node,
        overlay: Option<&TaskOverlay>,
        cross_background: bool,
        enter: &dyn Fn(TaskId) -> bool,
        first_live: bool,
    ) -> Vec<Reached> {
        let links = overlay.map(|o| o.links.get_or_init(|| self.overlay_links(o)));
        let mut pending = vec![start];
        let mut result = Vec::new();
        let mut visited = HashSet::new();
        while let Some(at) = pending.pop() {
            if !visited.insert(at) {
                continue;
            }
            for children in [self.below.get(&at), links.and_then(|l| l.get(&at))]
                .into_iter()
                .flatten()
            {
                for child in children {
                    match *child {
                        Node::Conversation(_) => pending.push(*child),
                        Node::Task(id) => {
                            let record = overlay
                                .and_then(|o| o.tasks.get(&id))
                                .or_else(|| self.live.get(&id));
                            let Some(node) = record
                                .map(|r| TaskNode::of(r))
                                .or_else(|| self.settled.get(&id).copied())
                            else {
                                continue;
                            };
                            let live = record
                                .filter(|r| r.state.status() != TaskStatus::Terminal)
                                .cloned();
                            let stop = first_live && live.is_some() && !node.background;
                            result.push(Reached { id, node, live });
                            if stop {
                                return result;
                            }
                            if (cross_background || !node.background) && enter(id) {
                                pending.push(*child);
                            }
                        }
                    }
                }
            }
        }
        result
    }
}
