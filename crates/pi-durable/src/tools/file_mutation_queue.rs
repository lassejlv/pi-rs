//! Process-local mutation slots, shared across environments with the same ID.
use crate::{
    Context, Result,
    env::{ExecutionEnv, FileErrorCode},
    storage::{StorageFuture, line::eager},
};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use tokio::sync::watch;

type Key = (String, PathBuf);
static QUEUES: LazyLock<Mutex<HashMap<Key, Arc<Slot>>>> = LazyLock::new(Default::default);
struct Slot {
    done: watch::Sender<bool>,
    previous: Option<Arc<Slot>>,
}
impl Drop for Slot {
    fn drop(&mut self) {
        // A long pending chain can become uniquely owned at its last release.
        // Unlink it iteratively instead of recursively dropping Arc fields.
        let mut previous = self.previous.take();
        while let Some(slot) = previous {
            match Arc::try_unwrap(slot) {
                Ok(mut slot) => previous = slot.previous.take(),
                Err(_) => break,
            }
        }
    }
}
impl Slot {
    fn finished(&self) -> bool {
        let mut slot = self;
        loop {
            if !*slot.done.borrow() {
                return false;
            }
            match &slot.previous {
                Some(previous) => slot = previous,
                None => return true,
            }
        }
    }
    async fn wait(self: Arc<Self>) {
        let mut slots = Vec::new();
        let mut slot = Some(self);
        while let Some(current) = slot {
            slot = current.previous.clone();
            slots.push(current);
        }
        for slot in slots.into_iter().rev() {
            let mut done = slot.done.subscribe();
            while !*done.borrow_and_update() {
                if done.changed().await.is_err() {
                    break;
                }
            }
        }
    }
}
struct Release {
    key: Key,
    slot: Arc<Slot>,
}
impl Drop for Release {
    fn drop(&mut self) {
        self.slot.done.send_replace(true);
        let mut queues = QUEUES.lock();
        if queues.get(&self.key).is_some_and(|tail| tail.finished()) {
            queues.remove(&self.key);
        }
    }
}
pub async fn canonical(
    env: &dyn crate::env::FileSystem,
    absolute: &Path,
    context: &Context,
) -> Result<PathBuf> {
    let mut path = absolute.to_path_buf();
    let mut suffix = Vec::new();
    let base = loop {
        match env.canonical_path(&path, context).await {
            Ok(path) => break path,
            Err(error) if error.code() == FileErrorCode::NotSupported => break path,
            Err(error) if error.code() == FileErrorCode::NotFound => {
                let parent = env
                    .join_path(&[path.clone(), "..".into()], context)
                    .await
                    .map_err(super::file_error)?;
                let text = path.to_string_lossy();
                let parent_text = parent.to_string_lossy();
                if parent == path || !text.starts_with(parent_text.as_ref()) {
                    break path;
                }
                let start = parent_text.len() + usize::from(!parent_text.ends_with(['/', '\\']));
                suffix.push(text.get(start..).unwrap_or_default().to_owned());
                path = parent;
            }
            Err(error) => return Err(super::file_error(error)),
        }
    };
    let mut result = base;
    for name in suffix.into_iter().rev() {
        result = env
            .join_path(&[result, name.into()], context)
            .await
            .map_err(super::file_error)?;
    }
    Ok(result)
}
pub fn with_file_mutation_queue<T: Send + 'static>(
    env: Arc<dyn ExecutionEnv>,
    path: PathBuf,
    operation: impl FnOnce() -> PinFuture<T> + Send + 'static,
    context: Context,
) -> StorageFuture<T> {
    eager(async move {
        let absolute = env
            .absolute_path(&path, &context)
            .await
            .map_err(super::file_error)?;
        let key = (
            env.id().to_owned(),
            canonical(env.as_ref(), &absolute, &context).await?,
        );
        let (slot, previous) = {
            let mut queues = QUEUES.lock();
            let previous = queues.get(&key).filter(|slot| !slot.finished()).cloned();
            let slot = Arc::new(Slot {
                done: watch::channel(false).0,
                previous: previous.clone(),
            });
            queues.insert(key.clone(), slot.clone());
            (slot, previous)
        };
        let _release = Release { key, slot };
        if let Some(previous) = previous {
            previous.wait().await;
        }
        operation().await
    })
}
pub type PinFuture<T> = std::pin::Pin<Box<dyn Future<Output = Result<T>> + Send + 'static>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::{FileSystem, NativeExecutionEnv};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::{Semaphore, oneshot};

    #[test]
    fn shutting_down_a_queued_runtime_cannot_bypass_an_active_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, b"x").unwrap();
        let env = Arc::new(NativeExecutionEnv::new(directory.path()).unwrap());
        let key = (env.id().to_owned(), std::fs::canonicalize(&path).unwrap());
        let first_runtime = tokio::runtime::Runtime::new().unwrap();
        let second_runtime = tokio::runtime::Runtime::new().unwrap();
        let third_runtime = tokio::runtime::Runtime::new().unwrap();
        let release = Arc::new(Semaphore::new(0));
        let held = release.clone();
        let (entered, reached) = oneshot::channel();
        first_runtime.block_on(async {
            let first = with_file_mutation_queue(
                env.clone(),
                path.clone(),
                move || {
                    Box::pin(async move {
                        let _ = entered.send(());
                        held.acquire().await.unwrap().forget();
                        Ok(())
                    })
                },
                Context::default(),
            );
            reached.await.unwrap();
            drop(first);
        });
        let second_entered = Arc::new(AtomicBool::new(false));
        let called = second_entered.clone();
        second_runtime.block_on(async {
            let second = with_file_mutation_queue(
                env.clone(),
                path.clone(),
                move || {
                    Box::pin(async move {
                        called.store(true, Ordering::Release);
                        Ok(())
                    })
                },
                Context::default(),
            );
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if QUEUES
                        .lock()
                        .get(&key)
                        .is_some_and(|tail| tail.previous.is_some())
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            drop(second);
        });
        drop(second_runtime);
        assert!(!second_entered.load(Ordering::Acquire));
        let third_entered = Arc::new(AtomicBool::new(false));
        let called = third_entered.clone();
        third_runtime.block_on(async {
            let third = with_file_mutation_queue(
                env,
                path,
                move || {
                    Box::pin(async move {
                        called.store(true, Ordering::Release);
                        Ok(())
                    })
                },
                Context::default(),
            );
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if QUEUES.lock().get(&key).is_some_and(|tail| {
                        tail.previous.as_ref().is_some_and(|p| p.previous.is_some())
                    }) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(!third_entered.load(Ordering::Acquire));
            release.add_permits(1);
            third.await.unwrap();
        });
        assert!(third_entered.load(Ordering::Acquire));
        assert!(!QUEUES.lock().contains_key(&key));
    }
}
