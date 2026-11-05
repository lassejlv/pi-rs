use crate::{
    storage::{StorageFuture, line::eager},
    *,
};
use indexmap::IndexMap;
use parking_lot::Mutex;
use std::{hash::Hash, sync::Arc};
struct Waiter<T> {
    send: tokio::sync::oneshot::Sender<Result<T>>,
    signal: Option<pi_chord::context::AbortSignal>,
}
struct State<K, T> {
    next: u64,
    sets: IndexMap<K, IndexMap<u64, Waiter<T>>>,
}
/// Keyed waits settle once by publication, failure, or caller cancellation.
pub struct Waiters<K, T> {
    state: Arc<Mutex<State<K, T>>>,
}
impl<K, T> Clone for Waiters<K, T> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}
impl<K: Eq + Hash + Clone + Send + 'static, T: Clone + Send + 'static> Default for Waiters<K, T> {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                next: 1,
                sets: IndexMap::new(),
            })),
        }
    }
}
impl<K: Eq + Hash + Clone + Send + 'static, T: Clone + Send + 'static> Waiters<K, T> {
    pub fn add(&self, key: K, context: &Context) -> StorageFuture<T> {
        if let Err(error) = context.check() {
            return crate::storage::ready(Err(error.into()));
        }
        let (send, receive) = tokio::sync::oneshot::channel();
        let signal = context.abort_signal().cloned();
        let id = {
            let mut state = self.state.lock();
            let id = state.next;
            state.next += 1;
            state.sets.entry(key.clone()).or_default().insert(
                id,
                Waiter {
                    send,
                    signal: signal.clone(),
                },
            );
            id
        };
        let state = self.state.clone();
        eager(async move {
            let result = if let Some(signal) = signal {
                // Resolve checks cancellation synchronously, so a delivered
                // result is authoritative even if cancellation is also ready.
                tokio::select! {biased;result=receive=>result.map_err(|_|Error::message("Waiter source was dropped"))?,_ = signal.cancelled()=>Err(Error::from(signal.reason().expect("cancelled signal has reason").waiter_error()))}
            } else {
                receive
                    .await
                    .map_err(|_| Error::message("Waiter source was dropped"))?
            };
            let mut state = state.lock();
            if let Some(set) = state.sets.get_mut(&key) {
                set.shift_remove(&id);
                if set.is_empty() {
                    state.sets.shift_remove(&key);
                }
            }
            result
        })
    }
    pub fn keys(&self) -> Vec<K> {
        let mut state = self.state.lock();
        state.sets.retain(|_, set| {
            let aborted = set
                .iter()
                .filter_map(|(id, waiter)| {
                    waiter
                        .signal
                        .as_ref()
                        .and_then(|signal| signal.check().err())
                        .map(|error| (*id, error))
                })
                .collect::<Vec<_>>();
            for (id, error) in aborted {
                if let Some(waiter) = set.shift_remove(&id) {
                    let _ = waiter.send.send(Err(error.into()));
                }
            }
            !set.is_empty()
        });
        state.sets.keys().cloned().collect()
    }
    pub fn resolve(&self, key: &K, value: T) {
        let set = self.state.lock().sets.shift_remove(key);
        for waiter in set.into_iter().flat_map(|s| s.into_values()) {
            let result = waiter
                .signal
                .as_ref()
                .map_or(Ok(()), |signal| signal.check())
                .map_err(Error::from)
                .map(|_| value.clone());
            let _ = waiter.send.send(result);
        }
    }
    pub fn reject_all(&self, error: Error) {
        let sets = std::mem::take(&mut self.state.lock().sets);
        for waiter in sets.into_values().flat_map(|s| s.into_values()) {
            let result = waiter
                .signal
                .as_ref()
                .map_or(Ok(()), |signal| signal.check())
                .map_err(Error::from)
                .and(Err(error.clone()));
            let _ = waiter.send.send(result);
        }
    }
}
pub async fn scan_all<T>(
    mut scan: impl FnMut(Option<Cursor>) -> StorageFuture<Page<T>>,
) -> Result<Vec<T>> {
    let mut result = Vec::new();
    let mut cursor = None;
    loop {
        let page = scan(cursor).await?;
        result.extend(page.items);
        cursor = page.next;
        if cursor.is_none() {
            return Ok(result);
        }
    }
}
pub fn closed_error(session: &Session) -> Error {
    session
        .failure()
        .map_or_else(|| Error::message("Harness is closed"), Error::failed)
}
