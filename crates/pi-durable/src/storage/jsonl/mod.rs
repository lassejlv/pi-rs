//! The main marker is the authority. Sidecars precede it; in-memory adoption
//! precedes retryable reclamation. Recovery accepts only confirmed records.
mod format;
mod recovery;
use crate::{
    env::FileSystem,
    storage::{
        Adopt, StorageFuture,
        line::{MutationLine, eager},
        ready,
    },
    *,
};
use format::*;
use indexmap::IndexMap;
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct JsonlStorageOptions {
    pub fsync: bool,
}
#[derive(Default)]
struct Metadata {
    closed: bool,
    poison: Option<Error>,
    current_only: HashSet<DocumentId>,
    live_tasks: HashSet<TaskId>,
}
struct Inner {
    fs: Arc<dyn FileSystem>,
    directory: PathBuf,
    main_path: PathBuf,
    options: JsonlStorageOptions,
    memory: MemoryStorage,
    metadata: Mutex<Metadata>,
    line: MutationLine,
}
#[derive(Clone)]
pub struct JsonlStorage {
    inner: Arc<Inner>,
}
struct EncodedCommit {
    marker: String,
    sidecars: IndexMap<String, String>,
}
impl JsonlStorage {
    pub fn open(
        directory: impl Into<PathBuf>,
        fs: Arc<dyn FileSystem>,
        context: &Context,
        options: JsonlStorageOptions,
    ) -> StorageFuture<Self> {
        let directory = directory.into();
        let context = context.clone();
        eager(async move {
            let directory = fs
                .absolute_path(&directory, &context)
                .await
                .map_err(|e| Error::jsonl_io("path resolution", e))?;
            fs.create_dir(&directory, true, &context)
                .await
                .map_err(|e| Error::jsonl_io("directory creation", e))?;
            let main_path = fs
                .join_path(&[directory.clone(), MAIN_FILE.into()], &context)
                .await
                .map_err(|e| Error::jsonl_io("path join", e))?;
            let storage = Self {
                inner: Arc::new(Inner {
                    fs,
                    directory,
                    main_path,
                    options,
                    memory: MemoryStorage::new(),
                    metadata: Mutex::new(Metadata::default()),
                    line: MutationLine::default(),
                }),
            };
            storage.recover(&context).await?;
            Ok(storage)
        })
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_native(
        directory: impl Into<PathBuf>,
        context: &Context,
        options: JsonlStorageOptions,
    ) -> StorageFuture<Self> {
        let fs = match crate::env::NativeFileSystem::new(".") {
            Ok(fs) => Arc::new(fs),
            Err(e) => return ready(Err(Error::jsonl_io("path resolution", e))),
        };
        Self::open(directory, fs, context, options)
    }
    pub fn directory(&self) -> &Path {
        &self.inner.directory
    }
    fn access(&self) -> Result<parking_lot::MutexGuard<'_, Metadata>> {
        let metadata = self.inner.metadata.lock();
        if metadata.closed {
            Err(Error::message("JsonlStorage is closed"))
        } else if let Some(error) = &metadata.poison {
            Err(error.clone())
        } else {
            Ok(metadata)
        }
    }
    fn poison(&self, cause: Error) -> Error {
        self.inner
            .metadata
            .lock()
            .poison
            .get_or_insert_with(|| Error::jsonl_poisoned(cause))
            .clone()
    }
    fn encode_commit(seq: Seq, writes: &[StorageWrite]) -> Result<EncodedCommit> {
        let mut main_writes = Vec::new();
        let mut sidecars: IndexMap<String, String> = IndexMap::new();
        let mut ordinal = 0;
        let mut add = |file: String, payload: SidecarPayload| {
            let current = ordinal;
            ordinal += 1;
            sidecars.entry(file).or_default().push_str(
                &SidecarRecord {
                    seq,
                    ordinal: current,
                    payload,
                }
                .line(),
            );
            current
        };
        for write in writes {
            match write {
                StorageWrite::Conversation { .. }
                | StorageWrite::Entry { .. }
                | StorageWrite::Submission { .. }
                | StorageWrite::DocumentRetire { .. } => {
                    main_writes.push(MainOperation::Direct(write.clone()))
                }
                StorageWrite::Task { value } if value.state.status() == TaskStatus::Terminal => {
                    main_writes.push(MainOperation::Direct(write.clone()))
                }
                StorageWrite::Task { value } => {
                    let id = value.id();
                    let ordinal = add(
                        file_name("task", id.get()),
                        SidecarPayload::Task(value.clone()),
                    );
                    main_writes.push(MainOperation::Task { id, ordinal });
                }
                StorageWrite::DocumentCreate { record, content } => {
                    let ordinal = add(
                        file_name("doc", record.id.get()),
                        SidecarPayload::Document {
                            id: record.id,
                            content: DocumentContent::Base {
                                version: content.version,
                                value: content.value.clone(),
                            },
                        },
                    );
                    main_writes.push(MainOperation::Create {
                        record: record.clone(),
                        ordinal,
                    });
                }
                StorageWrite::DocumentChange { id, content } => {
                    let ordinal = add(
                        file_name("doc", id.get()),
                        SidecarPayload::Document {
                            id: *id,
                            content: content.clone(),
                        },
                    );
                    main_writes.push(MainOperation::Change { id: *id, ordinal });
                }
                StorageWrite::DocumentCopy { .. } => {
                    return Err(Error::message(
                        "Document copy was not resolved before JSONL encoding",
                    ));
                }
            }
        }
        Ok(EncodedCommit {
            marker: MainMarker {
                seq,
                writes: main_writes,
            }
            .line(),
            sidecars,
        })
    }
    fn plan_reclamations(
        &self,
        writes: &[StorageWrite],
        encoded: &EncodedCommit,
    ) -> IndexMap<String, String> {
        let metadata = self.inner.metadata.lock();
        let mut created = HashSet::new();
        let mut retired = IndexMap::new();
        let mut bases = IndexMap::new();
        let mut tasks = IndexMap::new();
        for write in writes {
            match write {
                StorageWrite::DocumentCreate { record, .. } if record.semantics.current_only() => {
                    created.insert(record.id);
                }
                StorageWrite::DocumentRetire { id } => {
                    retired.insert(*id, ());
                }
                StorageWrite::DocumentChange {
                    id,
                    content: DocumentContent::Base { .. },
                } => {
                    bases.insert(*id, ());
                }
                StorageWrite::Task { value } => {
                    tasks.insert(value.id(), value.state.status());
                }
                _ => {}
            }
        }
        let current = |id: &DocumentId| metadata.current_only.contains(id) || created.contains(id);
        let mut replacements = IndexMap::new();
        for id in retired.keys().filter(|id| current(id)) {
            replacements.insert(file_name("doc", id.get()), String::new());
        }
        for id in bases
            .keys()
            .filter(|id| current(id) && !retired.contains_key(*id))
        {
            let file = file_name("doc", id.get());
            if let Some(content) = encoded.sidecars.get(&file) {
                replacements.insert(file, content.clone());
            }
        }
        for (id, status) in tasks {
            let file = file_name("task", id.get());
            if status == TaskStatus::Terminal
                && (metadata.live_tasks.contains(&id) || encoded.sidecars.contains_key(&file))
            {
                replacements.insert(file, String::new());
            }
        }
        replacements
    }
    fn adopt_sidecar_state(&self, writes: &[StorageWrite]) {
        let mut metadata = self.inner.metadata.lock();
        for write in writes {
            match write {
                StorageWrite::DocumentCreate { record, .. } if record.semantics.current_only() => {
                    metadata.current_only.insert(record.id);
                }
                StorageWrite::Task { value } => {
                    if value.state.status() == TaskStatus::Terminal {
                        metadata.live_tasks.remove(&value.id());
                    } else {
                        metadata.live_tasks.insert(value.id());
                    }
                }
                _ => {}
            }
        }
    }
    async fn resolve_file(&self, file: &str, context: &Context) -> Result<PathBuf> {
        self.inner
            .fs
            .join_path(&[self.inner.directory.clone(), file.into()], context)
            .await
            .map_err(|e| Error::jsonl_io("path join", e))
    }
    async fn reclaim_sidecars(&self, replacements: IndexMap<String, String>, context: &Context) {
        if replacements.is_empty() {
            return;
        }
        if self.inner.options.fsync
            && self
                .inner
                .fs
                .flush_file(&self.inner.main_path, context)
                .await
                .is_err()
        {
            return;
        }
        for (file, content) in replacements {
            let Ok(path) = self.resolve_file(&file, context).await else {
                continue;
            };
            if content.is_empty() {
                let _ = self.inner.fs.remove(&path, false, true, context).await;
                continue;
            }
            let Ok(temp) = self
                .resolve_file(&format!("{file}{RECLAIM_SUFFIX}"), context)
                .await
            else {
                continue;
            };
            if self
                .inner
                .fs
                .write_file(&temp, content.as_bytes(), context)
                .await
                .is_err()
            {
                continue;
            }
            if self.inner.options.fsync && self.inner.fs.flush_file(&temp, context).await.is_err() {
                continue;
            }
            let _ = self.inner.fs.rename_file(&temp, &path, context).await;
        }
    }
    async fn commit_on_line(
        &self,
        writes: Vec<StorageWrite>,
        adopt: Adopt,
        context: Context,
    ) -> Result<Seq> {
        if let Some(error) = &self.inner.metadata.lock().poison {
            return Err(error.clone());
        }
        let prepared = self.inner.memory.prepare_commit(&writes, None)?;
        let encoded = Self::encode_commit(prepared.seq(), prepared.writes())?;
        let replacements = self.plan_reclamations(prepared.writes(), &encoded);
        // Resolve every path before any append, keeping preparation failures usable.
        let paths = futures::future::try_join_all(
            encoded
                .sidecars
                .keys()
                .map(|file| self.resolve_file(file, &context)),
        )
        .await?;
        for ((file, content), path) in encoded.sidecars.iter().zip(&paths) {
            self.inner
                .fs
                .append_file(path, content.as_bytes(), &context)
                .await
                .map_err(|e| self.poison(Error::jsonl_io(format!("append to {file}"), e)))?;
        }
        if self.inner.options.fsync {
            for (file, path) in encoded.sidecars.keys().zip(&paths) {
                self.inner
                    .fs
                    .flush_file(path, &context)
                    .await
                    .map_err(|e| self.poison(Error::jsonl_io(format!("flush of {file}"), e)))?;
            }
        }
        self.inner
            .fs
            .append_file(&self.inner.main_path, encoded.marker.as_bytes(), &context)
            .await
            .map_err(|e| self.poison(Error::jsonl_io(format!("append to {MAIN_FILE}"), e)))?;
        let seq = prepared.apply()?;
        self.adopt_sidecar_state(prepared.writes());
        adopt(seq);
        self.reclaim_sidecars(replacements, &context).await;
        Ok(seq)
    }
}
macro_rules! forward {
    ($name:ident($($arg:ident:$ty:ty),*) -> $out:ty)=>{
        fn $name(&self,$($arg:$ty,)*context:&Context)->StorageFuture<$out>{
            let _access=match self.access(){Ok(access)=>access,Err(error)=>return ready(Err(error))};self.inner.memory.$name($($arg,)*context)
        }
    };
}
impl Storage for JsonlStorage {
    fn commit(
        &self,
        writes: &[StorageWrite],
        adopt: Adopt,
        context: &Context,
    ) -> StorageFuture<Seq> {
        let _access = match self.access() {
            Ok(access) => access,
            Err(error) => return ready(Err(error)),
        };
        let (storage, writes, context) = (self.clone(), writes.to_vec(), context.clone());
        self.inner
            .line
            .enqueue(async move { storage.commit_on_line(writes, adopt, context).await })
    }
    fn mint_id(&self) -> StorageFuture<i64> {
        let _access = match self.access() {
            Ok(access) => access,
            Err(error) => return ready(Err(error)),
        };
        self.inner.memory.mint_id()
    }
    forward!(conversation(id:ConversationId)->Option<ConversationRecord>);
    forward!(scan_conversations(query:&ConversationQuery,limit:usize,cursor:Option<&Cursor>)->Page<ConversationRecord>);
    forward!(entry(id:EntryId)->Option<crate::storage::StoredEntry>);
    forward!(entry_in(conversation:ConversationId,id:EntryId)->Option<crate::storage::StoredEntry>);
    forward!(find_latest_head_marker(conversation:ConversationId,at:Option<EntryId>)->Option<EntryRecord>);
    forward!(scan_entries(query:&EntryQuery,limit:usize,cursor:Option<&Cursor>)->Page<EntryRecord>);
    forward!(task(id:TaskId)->Option<TaskRecord>);
    forward!(scan_tasks(query:&TaskQuery,limit:usize,cursor:Option<&Cursor>)->Page<TaskRecord>);
    forward!(submission(id:SubmissionId)->Option<SubmissionRecord>);
    forward!(scan_submissions(query:&SubmissionQuery,limit:usize,cursor:Option<&Cursor>)->Page<SubmissionRecord>);
    forward!(submission_by_request(conversation:ConversationId,request:&JsonString)->Option<SubmissionRecord>);
    forward!(find_document(address:&DocumentAddress,at:DocumentPoint)->Option<DocumentRecord>);
    forward!(document(id:DocumentId,at:DocumentPoint)->Option<StoredDocument>);
    forward!(scan_documents(query:&DocumentQuery,limit:usize,cursor:Option<&Cursor>)->Page<DocumentRecord>);
    fn close(&self, context: &Context) -> StorageFuture<()> {
        let mut metadata = self.inner.metadata.lock();
        if metadata.closed {
            return ready(Ok(()));
        }
        metadata.closed = true;
        let (storage, context) = (self.clone(), context.clone());
        self.inner
            .line
            .enqueue(async move { storage.inner.memory.close(&context).await })
    }
}
