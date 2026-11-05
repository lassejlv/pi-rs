//! Typed executable task definitions. Persistence and dispatch use lossless JSON
//! at the boundary; handlers receive their native input and checkpoint types.
use crate::{
    harness::runtime::{TaskDriver, TaskRuntime},
    storage::StorageFuture,
    *,
};
use indexmap::IndexMap;
use serde::{Serialize, de::DeserializeOwned};
use std::{marker::PhantomData, sync::Arc};

pub trait TaskValue: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {}
impl<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> TaskValue for T {}

pub struct TypedTaskId<R> {
    raw: TaskId,
    marker: PhantomData<fn() -> R>,
}
impl<R> std::fmt::Debug for TypedTaskId<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.raw.fmt(f)
    }
}
impl<R> PartialEq for TypedTaskId<R> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl<R> Eq for TypedTaskId<R> {}
impl<R> std::hash::Hash for TypedTaskId<R> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}
impl<R> Copy for TypedTaskId<R> {}
impl<R> Clone for TypedTaskId<R> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<R> TypedTaskId<R> {
    pub fn from_untyped(raw: TaskId) -> Self {
        Self {
            raw,
            marker: PhantomData,
        }
    }
    pub fn untyped(self) -> TaskId {
        self.raw
    }
}
impl<R> Serialize for TypedTaskId<R> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.raw.serialize(serializer)
    }
}
impl<'de, R> serde::Deserialize<'de> for TypedTaskId<R> {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Ok(Self::from_untyped(serde::Deserialize::deserialize(
            deserializer,
        )?))
    }
}

/// A reserved running record. Construction checks the state once, so every
/// handler can read its checkpoint without matching scheduler-owned states.
#[derive(Clone, Debug)]
pub struct RunningTask<I = JsonValue, S = JsonValue, R = JsonValue> {
    record: TaskRecord<I, S, R>,
}
impl<I, S, R> RunningTask<I, S, R> {
    pub fn record(&self) -> &TaskRecord<I, S, R> {
        &self.record
    }
    pub fn checkpoint(&self) -> &S {
        match &self.record.state {
            TaskState::Running { checkpoint, .. } => checkpoint,
            _ => unreachable!("validated running record"),
        }
    }
    pub fn into_record(self) -> TaskRecord<I, S, R> {
        self.record
    }
    pub fn try_from_record(record: TaskRecord<I, S, R>) -> Result<Self> {
        if record.state.status() != TaskStatus::Running {
            return Err(Error::message(format!(
                "Task {} is not running",
                record.id()
            )));
        }
        Ok(Self { record })
    }
}

pub type PhaseHandler<I, S, R, H = ()> = Arc<
    dyn Fn(RunningTask<I, S, R>, TaskRuntime<I, S, R, H>, Context) -> StorageFuture<()>
        + Send
        + Sync,
>;
pub type TaskMigration<I, S> =
    Arc<dyn Fn(JsonValue, JsonValue, u64) -> Result<(I, S)> + Send + Sync>;
pub struct TaskDefinition<I, S, R, H = ()> {
    pub name: JsonString,
    pub version: u64,
    pub initial: Arc<dyn Fn(&I) -> Result<S> + Send + Sync>,
    pub phases: IndexMap<JsonString, PhaseHandler<I, S, R, H>>,
    pub abort: PhaseHandler<I, S, R, H>,
    pub migrate: Option<TaskMigration<I, S>>,
    pub hooks: Option<Arc<H>>,
}
pub struct Task<I, S, R, H = ()> {
    definition: Arc<TaskDefinition<I, S, R, H>>,
    erased: AnyTask,
}
impl<I, S, R, H> Clone for Task<I, S, R, H> {
    fn clone(&self) -> Self {
        Self {
            definition: self.definition.clone(),
            erased: self.erased.clone(),
        }
    }
}
impl<I, S, R, H> Task<I, S, R, H> {
    pub fn definition(&self) -> &TaskDefinition<I, S, R, H> {
        &self.definition
    }
    pub fn erased(&self) -> AnyTask {
        self.erased.clone()
    }
}
pub(crate) fn encode<T: Serialize>(value: &T) -> Result<JsonValue> {
    pi_chord::json::to_value(value).map_err(Error::message)
}
pub(crate) fn decode<T: DeserializeOwned>(value: &JsonValue) -> Result<T> {
    crate::wire::decode(value).map_err(Error::message)
}

type ErasedHandler =
    Arc<dyn Fn(TaskRecord, Arc<dyn TaskDriver>, Context) -> StorageFuture<()> + Send + Sync>;
pub(crate) struct Executable {
    pub name: JsonString,
    pub version: u64,
    pub initial: Arc<dyn Fn(&JsonValue) -> Result<JsonValue> + Send + Sync>,
    pub phases: IndexMap<JsonString, ErasedHandler>,
    pub abort: ErasedHandler,
    pub migrate: Option<TaskMigration<JsonValue, JsonValue>>,
}
#[derive(Clone)]
pub struct AnyTask(pub(crate) Arc<Executable>);
impl AnyTask {
    pub fn name(&self) -> &JsonString {
        &self.0.name
    }
    pub fn version(&self) -> u64 {
        self.0.version
    }
    pub fn same_definition(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn can_reserve(&self, record: &TaskRecord) -> bool {
        self.version() == record.info.version
            || (self.version() > record.info.version && self.0.migrate.is_some())
    }
}

pub fn define_task<I: TaskValue, S: TaskValue, R: TaskValue, H: Send + Sync + 'static>(
    definition: TaskDefinition<I, S, R, H>,
) -> Task<I, S, R, H> {
    fn erase<I: TaskValue, S: TaskValue, R: TaskValue, H: Send + Sync + 'static>(
        handler: PhaseHandler<I, S, R, H>,
    ) -> ErasedHandler {
        Arc::new(move |record, driver, context| {
            let handler = handler.clone();
            Box::pin(async move {
                let task = RunningTask::try_from_record(decode(&encode(&record)?)?)?;
                handler(task, TaskRuntime::new(driver), context).await
            })
        })
    }
    let definition = Arc::new(definition);
    let initial = definition.clone();
    let migrate = definition.migrate.clone().map(|migrate| {
        Arc::new(move |input, checkpoint, version| {
            let (input, checkpoint) = migrate(input, checkpoint, version)?;
            Ok((encode(&input)?, encode(&checkpoint)?))
        }) as TaskMigration<JsonValue, JsonValue>
    });
    let erased = AnyTask(Arc::new(Executable {
        name: definition.name.clone(),
        version: definition.version,
        initial: Arc::new(move |input| encode(&(initial.initial)(&decode(input)?)?)),
        phases: definition
            .phases
            .iter()
            .map(|(name, handler)| (name.clone(), erase(handler.clone())))
            .collect(),
        abort: erase(definition.abort.clone()),
        migrate,
    }));
    Task { definition, erased }
}

impl Transaction {
    pub fn create_erased_task(
        &self,
        task: &AnyTask,
        input: JsonValue,
        options: TaskOptions,
    ) -> StorageFuture<TaskId> {
        let initial = task.0.initial.clone();
        self.create_task_record(
            task.name().clone(),
            task.version(),
            input,
            move |input| initial(input),
            options,
        )
    }
    pub fn create_task<I: TaskValue, S: TaskValue, R: TaskValue, H: Send + Sync + 'static>(
        &self,
        task: &Task<I, S, R, H>,
        input: I,
        options: TaskOptions,
    ) -> StorageFuture<TypedTaskId<R>> {
        let input = match encode(&input) {
            Ok(input) => input,
            Err(error) => return crate::storage::ready(Err(error)),
        };
        let task = task.erased();
        let result = self.create_erased_task(&task, input, options);
        Box::pin(async move { Ok(TypedTaskId::from_untyped(result.await?)) })
    }
}
