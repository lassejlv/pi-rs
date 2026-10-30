//! Immutable task snapshots used by the scheduler. The harness registry builds
//! these from its built-ins and installed extensions.
use crate::*;
use indexmap::IndexMap;
use parking_lot::Mutex;
use std::sync::{Arc, Weak};

#[derive(Clone)]
pub struct InstalledTask {
    pub task: AnyTask,
    pub extension: Option<JsonString>,
}
#[derive(Clone, Default)]
pub struct TaskCatalogSnapshot {
    tasks: Arc<IndexMap<JsonString, InstalledTask>>,
}
impl TaskCatalogSnapshot {
    pub fn same_snapshot(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.tasks, &other.tasks)
    }
    pub(crate) fn build(tasks: impl IntoIterator<Item = InstalledTask>) -> Result<Self> {
        let mut next = IndexMap::new();
        for installed in tasks {
            let name = installed.task.name().clone();
            if next.contains_key(&name) {
                let extension = installed
                    .extension
                    .as_ref()
                    .map(|name| name.to_string())
                    .unwrap_or_default();
                return Err(Error::message(format!(
                    "Task {name} of extension {extension} is already installed"
                )));
            }
            next.insert(name, installed);
        }
        Ok(Self {
            tasks: Arc::new(next),
        })
    }
    pub fn task(&self, name: &JsonString) -> Option<&InstalledTask> {
        self.tasks.get(name)
    }
    pub fn tasks(&self) -> impl Iterator<Item = &InstalledTask> {
        self.tasks.values()
    }
}
type Listener = Arc<dyn Fn() + Send + Sync>;
#[derive(Default)]
struct State {
    snapshot: TaskCatalogSnapshot,
    listeners: IndexMap<u64, Listener>,
    next: u64,
}
#[derive(Clone, Default)]
pub struct TaskCatalog {
    state: Arc<Mutex<State>>,
}
pub struct CatalogSubscription {
    state: Weak<Mutex<State>>,
    id: u64,
}
impl Drop for CatalogSubscription {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state.lock().listeners.shift_remove(&self.id);
        }
    }
}
impl TaskCatalog {
    pub fn snapshot(&self) -> TaskCatalogSnapshot {
        self.state.lock().snapshot.clone()
    }
    pub fn subscribe(&self, listener: impl Fn() + Send + Sync + 'static) -> CatalogSubscription {
        let mut state = self.state.lock();
        state.next += 1;
        let id = state.next;
        state.listeners.insert(id, Arc::new(listener));
        CatalogSubscription {
            state: Arc::downgrade(&self.state),
            id,
        }
    }
    /// Validate the whole candidate before publishing. Existing phases retain
    /// the previous snapshot until their next durable phase boundary.
    pub fn replace(&self, tasks: impl IntoIterator<Item = InstalledTask>) -> Result<()> {
        let next = TaskCatalogSnapshot::build(tasks)?;
        let listeners = {
            let mut state = self.state.lock();
            state.snapshot = next;
            state.listeners.values().cloned().collect::<Vec<_>>()
        };
        for listener in listeners {
            listener();
        }
        Ok(())
    }
}
