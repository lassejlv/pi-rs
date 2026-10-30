use crate::{wire, *};
use pi_chord::delta::Op;
use serde::{Serialize, de::DeserializeOwned};
use std::{marker::PhantomData, sync::Arc};

#[derive(Clone, Copy, Debug)]
pub struct CheckpointInfo {
    pub deltas_since_base: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefinitionScope {
    Session,
    Task,
    Conversation { history: ConversationHistory },
}
impl DefinitionScope {
    pub fn matches(&self, record: &DocumentCreate) -> bool {
        match (self, &record.semantics) {
            (Self::Session, DocumentSemantics::Session)
            | (Self::Task, DocumentSemantics::Task { .. }) => true,
            (
                Self::Conversation { history: a },
                DocumentSemantics::Conversation { history: b, .. },
            ) => a == b,
            _ => false,
        }
    }
    fn semantics(&self, scope: &DocumentScope) -> Result<DocumentSemantics> {
        Ok(match (self, scope) {
            (Self::Session, DocumentScope::Session) => DocumentSemantics::Session,
            (Self::Task, DocumentScope::Task { task_id }) => {
                DocumentSemantics::Task { task_id: *task_id }
            }
            (Self::Conversation { history }, DocumentScope::Conversation { conversation_id }) => {
                DocumentSemantics::Conversation {
                    conversation_id: *conversation_id,
                    history: history.clone(),
                }
            }
            _ => {
                return Err(Error::message(
                    "Document address does not match its definition scope",
                ));
            }
        })
    }
}

mod sealed {
    pub trait Sealed {}
}
pub trait DocScope: sealed::Sealed + Clone + Send + Sync + 'static {
    type Owner: Copy + Send + Sync;
    fn definition_scope(&self) -> DefinitionScope;
    fn address_scope(owner: Self::Owner) -> DocumentScope;
}
#[derive(Clone, Copy, Debug)]
pub struct SessionScope;
#[derive(Clone, Copy, Debug)]
pub struct TaskScope;
#[derive(Clone, Copy, Debug)]
pub struct LatestConversationScope {
    pub fork: LatestFork,
}
#[derive(Clone, Copy, Debug)]
pub struct RewindableConversationScope {
    pub fork: ForkPolicy,
}
impl sealed::Sealed for SessionScope {}
impl sealed::Sealed for TaskScope {}
impl sealed::Sealed for LatestConversationScope {}
impl sealed::Sealed for RewindableConversationScope {}
impl DocScope for SessionScope {
    type Owner = ();
    fn definition_scope(&self) -> DefinitionScope {
        DefinitionScope::Session
    }
    fn address_scope(_: ()) -> DocumentScope {
        DocumentScope::Session
    }
}
impl DocScope for TaskScope {
    type Owner = TaskId;
    fn definition_scope(&self) -> DefinitionScope {
        DefinitionScope::Task
    }
    fn address_scope(task_id: TaskId) -> DocumentScope {
        DocumentScope::Task { task_id }
    }
}
impl DocScope for LatestConversationScope {
    type Owner = ConversationId;
    fn definition_scope(&self) -> DefinitionScope {
        DefinitionScope::Conversation {
            history: ConversationHistory::Latest { fork: self.fork },
        }
    }
    fn address_scope(conversation_id: ConversationId) -> DocumentScope {
        DocumentScope::Conversation { conversation_id }
    }
}
impl DocScope for RewindableConversationScope {
    type Owner = ConversationId;
    fn definition_scope(&self) -> DefinitionScope {
        DefinitionScope::Conversation {
            history: ConversationHistory::Rewindable { fork: self.fork },
        }
    }
    fn address_scope(conversation_id: ConversationId) -> DocumentScope {
        DocumentScope::Conversation { conversation_id }
    }
}
pub trait RewindableScope: DocScope {}
impl RewindableScope for RewindableConversationScope {}

type Initial = Arc<dyn Fn(Option<&JsonValue>) -> Result<JsonObject> + Send + Sync>;
type Migration = Arc<dyn Fn(JsonObject, u64) -> Result<JsonObject> + Send + Sync>;
type Checkpoint =
    Arc<dyn Fn(&JsonObject, &Arc<Vec<Op>>, CheckpointInfo) -> Result<bool> + Send + Sync>;
pub struct AnyDocDefinition {
    pub(crate) kind: JsonString,
    pub(crate) version: u64,
    pub(crate) scope: DefinitionScope,
    pub(crate) family: bool,
    initial: Initial,
    migrate: Option<Migration>,
    checkpoint: Checkpoint,
}
impl AnyDocDefinition {
    /// Validated dynamic definition boundary for embedded/runtime adapters.
    pub fn new(
        kind: impl Into<JsonString>,
        version: u64,
        scope: DefinitionScope,
        family: bool,
        initial: impl Fn(Option<&JsonValue>) -> Result<JsonObject> + Send + Sync + 'static,
        checkpoint: impl Fn(&JsonObject, &[Op], CheckpointInfo) -> Result<bool> + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::new_with_shared_checkpoint(
            kind,
            version,
            scope,
            family,
            initial,
            move |v, ops, info| checkpoint(v, ops, info),
        )
    }
    /// Runtime bindings can retain the exact prepared operation batch passed
    /// to persistence and publication, rather than copying a borrowed slice.
    pub fn new_with_shared_checkpoint(
        kind: impl Into<JsonString>,
        version: u64,
        scope: DefinitionScope,
        family: bool,
        initial: impl Fn(Option<&JsonValue>) -> Result<JsonObject> + Send + Sync + 'static,
        checkpoint: impl Fn(&JsonObject, &Arc<Vec<Op>>, CheckpointInfo) -> Result<bool>
        + Send
        + Sync
        + 'static,
    ) -> Result<Self> {
        let kind = kind.into();
        validate(&kind, version)?;
        Ok(Self {
            kind,
            version,
            scope,
            family,
            initial: Arc::new(initial),
            migrate: None,
            checkpoint: Arc::new(checkpoint),
        })
    }
    pub fn with_migration(
        mut self,
        migrate: impl Fn(JsonObject, u64) -> Result<JsonObject> + Send + Sync + 'static,
    ) -> Self {
        self.migrate = Some(Arc::new(migrate));
        self
    }
    pub fn check_address(&self, address: &DocumentAddress) -> Result<()> {
        if address.kind != self.kind || address.key.is_some() != self.family {
            return Err(Error::message(
                "Document address does not match its definition",
            ));
        }
        self.scope.semantics(&address.scope).map(|_| ())
    }
    pub fn kind(&self) -> &JsonString {
        &self.kind
    }
    pub fn version(&self) -> u64 {
        self.version
    }
    pub fn scope(&self) -> &DefinitionScope {
        &self.scope
    }
    pub fn is_family(&self) -> bool {
        self.family
    }

    pub fn initial(&self, seed: Option<&JsonValue>) -> Result<JsonObject> {
        (self.initial)(seed)
    }
    pub fn checkpoint_when(
        &self,
        value: &JsonObject,
        ops: &[Op],
        info: CheckpointInfo,
    ) -> Result<bool> {
        self.checkpoint_when_shared(value, &Arc::new(ops.to_vec()), info)
    }
    pub fn checkpoint_when_shared(
        &self,
        value: &JsonObject,
        ops: &Arc<Vec<Op>>,
        info: CheckpointInfo,
    ) -> Result<bool> {
        (self.checkpoint)(value, ops, info)
    }
    pub fn has_migration(&self) -> bool {
        self.migrate.is_some()
    }
    pub fn materialize(&self, stored: StoredDocument) -> Result<JsonObject> {
        self.materialize_value(&stored.record.create, stored.version, stored.value)
    }
    pub fn materialize_value(
        &self,
        record: &DocumentCreate,
        version: u64,
        value: JsonObject,
    ) -> Result<JsonObject> {
        self.check_record_scope(record)?;
        self.check_record_version(record, version)?;
        if version == self.version {
            Ok(value)
        } else {
            (self.migrate.as_ref().expect("migration checked"))(value, version)
        }
    }
    pub fn check_record_scope(&self, record: &DocumentCreate) -> Result<()> {
        if self.scope.matches(record) {
            Ok(())
        } else {
            Err(Error::message(format!(
                "Document {} ({}) does not match the supplied definition semantics",
                record.id, record.kind
            )))
        }
    }
    pub fn check_record_version(&self, record: &DocumentCreate, version: u64) -> Result<()> {
        if version > self.version {
            return Err(Error::message(format!(
                "Document {} ({}) has newer version {version} than {}",
                record.id, record.kind, self.version
            )));
        }
        if version < self.version && self.migrate.is_none() {
            return Err(Error::message(format!(
                "Document {} ({}) requires migration from version {version}",
                record.id, record.kind
            )));
        }
        Ok(())
    }
    pub fn create(&self, address: &DocumentAddress, id: DocumentId) -> Result<DocumentCreate> {
        Ok(DocumentCreate {
            id,
            kind: address.kind.clone(),
            key: address.key.clone(),
            semantics: self.scope.semantics(&address.scope)?,
        })
    }
}

/// Typed object state. JSON decoding validates typed placements at the boundary;
/// the erased runtime retains an object root and preserves arbitrary JSON data.
pub trait DocumentState: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Decode this state from its stored JSON. Untyped states override it to avoid a text round trip.
    fn from_state(value: &JsonValue) -> Result<Self> {
        wire::decode(value).map_err(Error::message)
    }
    /// Encode this state as JSON. Untyped states override it to avoid a text round trip.
    fn to_state(&self) -> Result<JsonValue> {
        pi_chord::json::to_value(self).map_err(Error::message)
    }
}
impl DocumentState for JsonObject {
    fn from_state(value: &JsonValue) -> Result<Self> {
        JsonObject::try_from_value(value.clone()).map_err(Error::message)
    }
    fn to_state(&self) -> Result<JsonValue> {
        Ok(self.value().clone())
    }
}
type TypedMigration<T> = Arc<dyn Fn(JsonObject, u64) -> Result<T> + Send + Sync>;
type RawCheckpoint = Arc<dyn Fn(&JsonObject, &[Op], CheckpointInfo) -> bool + Send + Sync>;
type TypedCheckpoint<T> = Arc<dyn Fn(&T, &[Op], CheckpointInfo) -> bool + Send + Sync>;
pub struct DocDefinition<T, S> {
    kind: JsonString,
    version: u64,
    scope: S,
    initial: Arc<dyn Fn() -> Result<T> + Send + Sync>,
    migrate: Option<TypedMigration<T>>,
    checkpoint: Option<TypedCheckpoint<T>>,
    raw_checkpoint: Option<RawCheckpoint>,
}
impl<T, S> DocDefinition<T, S> {
    pub fn new(
        kind: impl Into<JsonString>,
        version: u64,
        scope: S,
        initial: impl Fn() -> Result<T> + Send + Sync + 'static,
        checkpoint: impl Fn(&T, &[Op], CheckpointInfo) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: kind.into(),
            version,
            scope,
            initial: Arc::new(initial),
            migrate: None,
            checkpoint: Some(Arc::new(checkpoint)),
            raw_checkpoint: None,
        }
    }
    pub fn with_migration(
        mut self,
        migrate: impl Fn(JsonObject, u64) -> Result<T> + Send + Sync + 'static,
    ) -> Self {
        self.migrate = Some(Arc::new(migrate));
        self
    }
    /// A document that never asks for a checkpoint. Unlike `|_, _, _| false`, this skips decoding the whole
    /// document into `T` after every change, which costs time proportional to its size per commit.
    pub fn never_checkpoint(mut self) -> Self {
        self.checkpoint = None;
        self.raw_checkpoint = None;
        self
    }
    /// Decide checkpoints from the stored JSON itself. Replaces the typed predicate, so a commit never decodes the
    /// whole document just to answer it.
    pub fn raw_checkpoint(
        mut self,
        checkpoint: impl Fn(&JsonObject, &[Op], CheckpointInfo) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.raw_checkpoint = Some(Arc::new(checkpoint));
        self
    }
}
pub struct DocFamilyDefinition<T, I, S> {
    kind: JsonString,
    version: u64,
    scope: S,
    initial: Arc<dyn Fn(I) -> Result<T> + Send + Sync>,
    migrate: Option<TypedMigration<T>>,
    checkpoint: Option<TypedCheckpoint<T>>,
}
impl<T, I, S> DocFamilyDefinition<T, I, S> {
    pub fn new(
        kind: impl Into<JsonString>,
        version: u64,
        scope: S,
        initial: impl Fn(I) -> Result<T> + Send + Sync + 'static,
        checkpoint: impl Fn(&T, &[Op], CheckpointInfo) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: kind.into(),
            version,
            scope,
            initial: Arc::new(initial),
            migrate: None,
            checkpoint: Some(Arc::new(checkpoint)),
        }
    }
    pub fn with_migration(
        mut self,
        migrate: impl Fn(JsonObject, u64) -> Result<T> + Send + Sync + 'static,
    ) -> Self {
        self.migrate = Some(Arc::new(migrate));
        self
    }
    /// A document that never asks for a checkpoint. Unlike `|_, _, _| false`, this skips decoding the whole
    /// document into `T` after every change, which costs time proportional to its size per commit.
    pub fn never_checkpoint(mut self) -> Self {
        self.checkpoint = None;
        self
    }
}

pub struct DocToken<T, S> {
    pub(crate) definition: Arc<AnyDocDefinition>,
    marker: PhantomData<fn() -> (T, S)>,
}
pub struct DocFamilyToken<T, I, S> {
    pub(crate) definition: Arc<AnyDocDefinition>,
    marker: PhantomData<fn() -> (T, I, S)>,
}
impl<T, S> Clone for DocToken<T, S> {
    fn clone(&self) -> Self {
        Self {
            definition: self.definition.clone(),
            marker: PhantomData,
        }
    }
}
impl<T, I, S> Clone for DocFamilyToken<T, I, S> {
    fn clone(&self) -> Self {
        Self {
            definition: self.definition.clone(),
            marker: PhantomData,
        }
    }
}
impl<T, S: DocScope> DocToken<T, S> {
    pub fn definition(&self) -> &Arc<AnyDocDefinition> {
        &self.definition
    }
    /// The owner is fixed by the definition's scope.
    /// ```compile_fail
    /// use pi_durable::{JsonObject,ConversationId,documents::TaskDocToken};
    /// fn wrong_owner(doc: &TaskDocToken<JsonObject>, conversation: ConversationId) {
    ///     doc.address(conversation);
    /// }
    /// ```
    pub fn address(&self, owner: S::Owner) -> DocumentAddress {
        DocumentAddress {
            kind: self.definition.kind.clone(),
            scope: S::address_scope(owner),
            key: None,
        }
    }
}
impl<T, I, S: DocScope> DocFamilyToken<T, I, S> {
    pub fn definition(&self) -> &Arc<AnyDocDefinition> {
        &self.definition
    }

    pub fn address(&self, owner: S::Owner, key: impl Into<JsonString>) -> DocumentAddress {
        DocumentAddress {
            kind: self.definition.kind.clone(),
            scope: S::address_scope(owner),
            key: Some(key.into()),
        }
    }
}

fn validate(kind: &JsonString, version: u64) -> Result<()> {
    if version == 0 || version > MAX_SAFE_INTEGER as u64 {
        Err(Error::message(format!(
            "Document {kind} version must be a positive integer"
        )))
    } else {
        Ok(())
    }
}
fn to_object<T: Serialize>(value: &T) -> Result<JsonObject> {
    JsonObject::try_from_value(pi_chord::json::to_value(value).map_err(Error::message)?)
        .map_err(Error::message)
}
pub fn define_doc<T: DocumentState, S: DocScope>(
    definition: DocDefinition<T, S>,
) -> Result<DocToken<T, S>> {
    validate(&definition.kind, definition.version)?;
    let initial = definition.initial;
    let checkpoint = definition.checkpoint;
    let raw_checkpoint = definition.raw_checkpoint;
    let migrate = definition.migrate.map(|migrate| {
        Arc::new(move |value, version| to_object(&migrate(value, version)?)) as Migration
    });
    Ok(DocToken {
        definition: Arc::new(AnyDocDefinition {
            kind: definition.kind,
            version: definition.version,
            scope: definition.scope.definition_scope(),
            family: false,
            initial: Arc::new(move |_| to_object(&initial()?)),
            migrate,
            checkpoint: match (raw_checkpoint, checkpoint) {
                (Some(raw), _) => Arc::new(move |value, ops, info| Ok(raw(value, ops, info))),
                (None, Some(checkpoint)) => Arc::new(move |value, ops, info| {
                    Ok(checkpoint(&T::from_state(value.value())?, ops, info))
                }),
                (None, None) => Arc::new(|_, _, _| Ok(false)),
            },
        }),
        marker: PhantomData,
    })
}
pub fn define_doc_family<
    T: DocumentState,
    I: Serialize + DeserializeOwned + Send + Sync + 'static,
    S: DocScope,
>(
    definition: DocFamilyDefinition<T, I, S>,
) -> Result<DocFamilyToken<T, I, S>> {
    validate(&definition.kind, definition.version)?;
    let initial = definition.initial;
    let checkpoint = definition.checkpoint;
    let migrate = definition.migrate.map(|migrate| {
        Arc::new(move |value, version| to_object(&migrate(value, version)?)) as Migration
    });
    Ok(DocFamilyToken {
        definition: Arc::new(AnyDocDefinition {
            kind: definition.kind,
            version: definition.version,
            scope: definition.scope.definition_scope(),
            family: true,
            initial: Arc::new(move |seed| {
                let seed =
                    seed.ok_or_else(|| Error::message("Document family requires a creation seed"))?;
                to_object(&initial(wire::decode(seed).map_err(Error::message)?)?)
            }),
            migrate,
            checkpoint: match checkpoint {
                Some(checkpoint) => Arc::new(move |value, ops, info| {
                    Ok(checkpoint(&T::from_state(value.value())?, ops, info))
                }),
                None => Arc::new(|_, _, _| Ok(false)),
            },
        }),
        marker: PhantomData,
    })
}

pub type SessionDocToken<T> = DocToken<T, SessionScope>;
pub type TaskDocToken<T> = DocToken<T, TaskScope>;
pub type LatestConversationDocToken<T> = DocToken<T, LatestConversationScope>;
pub type RewindableConversationDocToken<T> = DocToken<T, RewindableConversationScope>;
pub type SessionDocFamilyToken<T, I> = DocFamilyToken<T, I, SessionScope>;
pub type TaskDocFamilyToken<T, I> = DocFamilyToken<T, I, TaskScope>;
pub type LatestConversationDocFamilyToken<T, I> = DocFamilyToken<T, I, LatestConversationScope>;
pub type RewindableConversationDocFamilyToken<T, I> =
    DocFamilyToken<T, I, RewindableConversationScope>;

pub fn address_id(address: &DocumentAddress) -> String {
    let (kind, owner) = match address.scope {
        DocumentScope::Session => ("session", JsonValue::Null),
        DocumentScope::Conversation { conversation_id } => {
            ("conversation", wire::encode(&conversation_id))
        }
        DocumentScope::Task { task_id } => ("task", wire::encode(&task_id)),
    };
    pi_chord::json::stringify(&JsonValue::Array(
        vec![
            JsonValue::String(address.kind.clone()),
            kind.into(),
            owner,
            address
                .key
                .as_ref()
                .map_or(JsonValue::Null, |key| JsonValue::String(key.clone())),
        ]
        .into(),
    ))
}
pub struct ResolvedAddress {
    pub address: DocumentAddress,
    pub id: String,
    pub next_argument: usize,
}
pub fn resolve_address(
    definition: &AnyDocDefinition,
    args: &[JsonValue],
) -> Result<ResolvedAddress> {
    let mut index = 0;
    let scope = match definition.scope {
        DefinitionScope::Session => DocumentScope::Session,
        _ => {
            let kind = if matches!(definition.scope, DefinitionScope::Task) {
                "task"
            } else {
                "conversation"
            };
            let owner = args
                .first()
                .and_then(|value| wire::decode::<ConversationId>(value).ok())
                .ok_or_else(|| {
                    Error::message(format!("Document {} requires a {kind} ID", definition.kind))
                })?;
            index += 1;
            if kind == "task" {
                DocumentScope::Task {
                    task_id: TaskId::from_raw(owner.get()),
                }
            } else {
                DocumentScope::Conversation {
                    conversation_id: owner,
                }
            }
        }
    };
    let key = if definition.family {
        let key = args
            .get(index)
            .map(|value| wire::decode::<JsonString>(value).map_err(Error::message))
            .transpose()?;
        index += 1;
        key
    } else {
        None
    };
    let address = DocumentAddress {
        kind: definition.kind.clone(),
        scope,
        key,
    };
    Ok(ResolvedAddress {
        id: address_id(&address),
        address,
        next_argument: index,
    })
}
