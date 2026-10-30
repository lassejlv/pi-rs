//! Published extension snapshots. Built-in definitions are supplied by the
//! harness so the registry never substitutes names for executable tasks.
use super::catalog::{InstalledTask, TaskCatalogSnapshot};
use crate::*;
use indexmap::{IndexMap, IndexSet};
use parking_lot::Mutex;
use std::{
    any::Any,
    sync::{Arc, Weak},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCaller {
    Model,
    Tools,
}
pub trait RegisteredTool: Send + Sync + 'static {
    fn name(&self) -> &JsonString;
    fn callers(&self) -> Option<&[ToolCaller]>;
    fn deferred(&self) -> bool;
    fn executable(self: Arc<Self>) -> Option<Arc<dyn super::tool::ExecutableTool>> {
        None
    }
}
pub trait RegisteredSection: Send + Sync + 'static {
    fn key(&self) -> &JsonString;
}
pub enum Wrap<T, S> {
    Tool {
        name: JsonString,
        apply: Arc<dyn Fn(Arc<T>) -> Result<Arc<T>> + Send + Sync>,
    },
    Section {
        key: JsonString,
        apply: Arc<dyn Fn(Arc<S>) -> Result<Arc<S>> + Send + Sync>,
    },
}
pub struct HookRegistration {
    pub task: JsonString,
    pub handlers: Arc<dyn Any + Send + Sync>,
}
impl HookRegistration {
    pub fn for_task<I, S, R, H: Send + Sync + 'static>(
        task: &Task<I, S, R, H>,
        handlers: Arc<H>,
    ) -> Self {
        Self::new(task.definition().name.clone(), handlers)
    }
    pub fn new<H: Send + Sync + 'static>(task: impl Into<JsonString>, handlers: Arc<H>) -> Self {
        Self {
            task: task.into(),
            handlers,
        }
    }
}
pub struct Extension<T, S> {
    pub name: JsonString,
    pub tools: Vec<Arc<T>>,
    pub sections: Vec<Arc<S>>,
    pub tasks: Vec<AnyTask>,
    pub hooks: Vec<HookRegistration>,
    pub wraps: Vec<Wrap<T, S>>,
}
impl<T, S> Extension<T, S> {
    pub fn new(name: impl Into<JsonString>) -> Self {
        Self {
            name: name.into(),
            tools: Vec::new(),
            sections: Vec::new(),
            tasks: Vec::new(),
            hooks: Vec::new(),
            wraps: Vec::new(),
        }
    }
}
pub struct RegistrySnapshot<T, S> {
    inner: Arc<Snapshot<T, S>>,
}
struct Snapshot<T, S> {
    extensions: Vec<Arc<Extension<T, S>>>,
    by_name: IndexMap<JsonString, Arc<Extension<T, S>>>,
    tasks: TaskCatalogSnapshot,
}
impl<T, S> Clone for RegistrySnapshot<T, S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
impl<T, S> RegistrySnapshot<T, S> {
    pub fn installed(&self) -> &[Arc<Extension<T, S>>] {
        &self.inner.extensions
    }
    pub fn extension(&self, name: &JsonString) -> Option<Arc<Extension<T, S>>> {
        self.inner.by_name.get(name).cloned()
    }
    pub fn task(&self, name: &JsonString) -> Option<AnyTask> {
        self.inner.tasks.task(name).map(|task| task.task.clone())
    }
    pub fn tasks(&self) -> Vec<AnyTask> {
        self.inner
            .tasks
            .tasks()
            .map(|task| task.task.clone())
            .collect()
    }
    pub fn tools(&self) -> Vec<(Arc<Extension<T, S>>, Arc<T>)> {
        self.installed()
            .iter()
            .flat_map(|extension| {
                extension
                    .tools
                    .iter()
                    .cloned()
                    .map(|tool| (extension.clone(), tool))
            })
            .collect()
    }
    pub fn sections(&self) -> Vec<(Arc<Extension<T, S>>, Arc<S>)> {
        self.installed()
            .iter()
            .flat_map(|extension| {
                extension
                    .sections
                    .iter()
                    .cloned()
                    .map(|section| (extension.clone(), section))
            })
            .collect()
    }
    pub fn task_catalog(&self) -> TaskCatalogSnapshot {
        self.inner.tasks.clone()
    }
    pub fn same_snapshot(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
    fn build(extensions: Vec<Arc<Extension<T, S>>>, builtins: &[AnyTask]) -> Result<Self> {
        let tasks = builtins
            .iter()
            .cloned()
            .map(|task| InstalledTask {
                task,
                extension: None,
            })
            .chain(extensions.iter().flat_map(|extension| {
                extension.tasks.iter().cloned().map(|task| InstalledTask {
                    task,
                    extension: Some(extension.name.clone()),
                })
            }))
            .collect::<Vec<_>>();
        let tasks = TaskCatalogSnapshot::build(tasks)?;
        let by_name = extensions
            .iter()
            .map(|extension| (extension.name.clone(), extension.clone()))
            .collect();
        Ok(Self {
            inner: Arc::new(Snapshot {
                extensions,
                by_name,
                tasks,
            }),
        })
    }
}
type Listener = Arc<dyn Fn() -> Result<()> + Send + Sync>;
struct State<T, S> {
    current: RegistrySnapshot<T, S>,
    listeners: IndexMap<u64, Listener>,
    next: u64,
}
struct Inner<T, S> {
    state: Mutex<State<T, S>>,
    builtins: Vec<AnyTask>,
}
pub struct Registry<T, S> {
    inner: Arc<Inner<T, S>>,
}
impl<T, S> Clone for Registry<T, S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
pub struct RegistrySubscription<T, S> {
    inner: Weak<Inner<T, S>>,
    id: u64,
}
impl<T, S> Drop for RegistrySubscription<T, S> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.state.lock().listeners.shift_remove(&self.id);
        }
    }
}
impl<T: RegisteredTool, S: RegisteredSection> Registry<T, S> {
    pub fn new(builtins: impl IntoIterator<Item = AnyTask>) -> Result<Self> {
        let builtins = builtins.into_iter().collect::<Vec<_>>();
        let current = RegistrySnapshot::build(Vec::new(), &builtins)?;
        Ok(Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    current,
                    listeners: IndexMap::new(),
                    next: 0,
                }),
                builtins,
            }),
        })
    }
    pub fn snapshot(&self) -> RegistrySnapshot<T, S> {
        self.inner.state.lock().current.clone()
    }
    pub fn subscribe(
        &self,
        listener: impl Fn() + Send + Sync + 'static,
    ) -> RegistrySubscription<T, S> {
        self.subscribe_fallible(move || {
            listener();
            Ok(())
        })
    }
    /// The snapshot is already published when a listener runs. A failing
    /// listener stops delivery, while retaining the published state.
    pub fn subscribe_fallible(
        &self,
        listener: impl Fn() -> Result<()> + Send + Sync + 'static,
    ) -> RegistrySubscription<T, S> {
        let mut state = self.inner.state.lock();
        state.next += 1;
        let id = state.next;
        state.listeners.insert(id, Arc::new(listener));
        RegistrySubscription {
            inner: Arc::downgrade(&self.inner),
            id,
        }
    }
    pub fn install(&self, extension: Arc<Extension<T, S>>) -> Result<()> {
        validate_extension(&extension)?;
        let listeners = {
            let mut state = self.inner.state.lock();
            let mut next = state.current.installed().to_vec();
            if let Some(index) = next
                .iter()
                .position(|installed| installed.name == extension.name)
            {
                next[index] = extension;
            } else {
                next.push(extension);
            }
            let candidate = RegistrySnapshot::build(next, &self.inner.builtins)?;
            state.current = candidate;
            state.listeners.values().cloned().collect::<Vec<_>>()
        };
        for listener in listeners {
            listener()?;
        }
        Ok(())
    }
    pub fn uninstall(&self, name: &JsonString) -> Result<()> {
        let listeners = {
            let mut state = self.inner.state.lock();
            if state.current.extension(name).is_none() {
                return Ok(());
            }
            let next = state
                .current
                .installed()
                .iter()
                .filter(|extension| extension.name != *name)
                .cloned()
                .collect();
            state.current = RegistrySnapshot::build(next, &self.inner.builtins)?;
            state.listeners.values().cloned().collect::<Vec<_>>()
        };
        for listener in listeners {
            listener()?;
        }
        Ok(())
    }
}
fn validate_extension<T: RegisteredTool, S: RegisteredSection>(
    extension: &Extension<T, S>,
) -> Result<()> {
    let mut tools = IndexSet::new();
    for tool in &extension.tools {
        if !tools.insert(tool.name()) {
            return Err(Error::message(format!(
                "Extension {} has two tools named {}",
                extension.name,
                tool.name()
            )));
        }
    }
    let mut sections = IndexSet::new();
    for section in &extension.sections {
        let key = section.key();
        let units = key.units();
        let valid = units
            .first()
            .is_some_and(|unit| (*unit >= 97) && (*unit <= 122))
            && units
                .iter()
                .skip(1)
                .all(|unit| matches!(*unit,97..=122|48..=57|95|45));
        if !valid {
            return Err(pi_chord::delta::Error::Type(format!(
                "Section key {} must match /^[a-z][a-z0-9_-]*$/",
                crate::wire::encode(key)
            ))
            .into());
        }
        if key == &JsonString::from("instructions") {
            return Err(Error::message(
                "Section key instructions is reserved for the agent's instructions",
            ));
        }
        if !sections.insert(key) {
            return Err(Error::message(format!(
                "Extension {} has two sections with key {key}",
                extension.name
            )));
        }
    }
    Ok(())
}
