//! Lazy native image worker with serialized requests and idle retirement.
use super::*;
use crate::{Error, storage::StorageFuture};
use futures::channel::oneshot;
use parking_lot::Mutex;
use std::{
    sync::{Weak, mpsc},
    time::Duration,
};
#[derive(Clone, Copy, Debug)]
pub struct NodePhotonImagesOptions {
    pub worker: bool,
    pub idle_ms: f64,
}
impl Default for NodePhotonImagesOptions {
    fn default() -> Self {
        Self {
            worker: true,
            idle_ms: 30000.0,
        }
    }
}
struct Request {
    bytes: Vec<u8>,
    mime: String,
    limits: ImageLimits,
    reply: oneshot::Sender<Option<PreparedImage>>,
}
struct Worker {
    id: u64,
    send: mpsc::Sender<Request>,
}
#[derive(Default)]
struct State {
    worker: Option<Worker>,
    next: u64,
    fallback: bool,
}
pub struct NativePhotonImages {
    options: NodePhotonImagesOptions,
    state: Arc<Mutex<State>>,
}
pub fn create_native_photon_images(options: NodePhotonImagesOptions) -> Arc<NativePhotonImages> {
    Arc::new(NativePhotonImages {
        options,
        state: Arc::new(Mutex::new(State::default())),
    })
}
fn interval(ms: f64) -> Duration {
    Duration::from_millis(
        if !ms.is_finite() || !(1.0..=2_147_483_647.0).contains(&ms) {
            1
        } else {
            ms.floor() as u64
        },
    )
}
fn worker(state: Weak<Mutex<State>>, id: u64, receive: mpsc::Receiver<Request>, idle: Duration) {
    loop {
        let request = match receive.recv_timeout(idle) {
            Ok(request) => request,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let Some(owner) = state.upgrade() else {
                    return;
                };
                let mut state = owner.lock();
                match receive.try_recv() {
                    Ok(request) => request,
                    Err(_) => {
                        if state.worker.as_ref().is_some_and(|w| w.id == id) {
                            state.worker = None;
                        }
                        return;
                    }
                }
            }
        };
        let result = super::prepare(request.bytes, request.mime, request.limits);
        let _ = request.reply.send(result);
    }
}
impl ImageProcessor for NativePhotonImages {
    fn prepare(
        &self,
        bytes: Vec<u8>,
        mime: String,
        limits: ImageLimits,
    ) -> StorageFuture<Option<PreparedImage>> {
        if !self.options.worker {
            let result = super::prepare(bytes, mime, limits);
            return Box::pin(async move { Ok(result) });
        }
        let mut state = self.state.lock();
        if state.fallback {
            drop(state);
            let result = super::prepare(bytes, mime, limits);
            return Box::pin(async move { Ok(result) });
        }
        if state.worker.is_none() {
            let id = state.next;
            state.next += 1;
            let (send, receive) = mpsc::channel();
            let weak = Arc::downgrade(&self.state);
            let idle = interval(self.options.idle_ms);
            match std::thread::Builder::new()
                .name("pi-images".into())
                .spawn(move || worker(weak, id, receive, idle))
            {
                Ok(_) => state.worker = Some(Worker { id, send }),
                Err(_) => {
                    state.fallback = true;
                    drop(state);
                    let result = super::prepare(bytes, mime, limits);
                    return Box::pin(async move { Ok(result) });
                }
            }
        }
        let (send, receive) = oneshot::channel();
        let request = Request {
            bytes,
            mime,
            limits,
            reply: send,
        };
        if let Err(error) = state.worker.as_ref().unwrap().send.send(request) {
            state.worker = None;
            state.fallback = true;
            drop(state);
            let request = error.0;
            let result = super::prepare(request.bytes, request.mime, request.limits);
            return Box::pin(async move { Ok(result) });
        }
        Box::pin(async move {
            receive
                .await
                .map_err(|_| Error::message("The Photon image worker stopped before replying"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    #[test]
    fn worker_is_lazy_retires_when_idle_and_finishes_after_its_public_owner_drops() {
        let bytes = STANDARD
            .decode("R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7")
            .unwrap();
        let images = create_native_photon_images(NodePhotonImagesOptions {
            worker: true,
            idle_ms: 10.0,
        });
        assert!(images.state.lock().worker.is_none());
        let prepared = futures::executor::block_on(images.prepare(
            bytes.clone(),
            "image/gif".into(),
            DEFAULT_IMAGE_LIMITS,
        ))
        .unwrap()
        .unwrap();
        assert_eq!(prepared.data, to_base64(&bytes));
        assert_eq!(images.state.lock().next, 1);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while images.state.lock().worker.is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "idle image worker did not retire"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            futures::executor::block_on(images.prepare(
                vec![1, 2, 3],
                "image/png".into(),
                DEFAULT_IMAGE_LIMITS
            ))
            .unwrap()
            .is_none()
        );
        assert_eq!(images.state.lock().next, 2);
        let pending = images.prepare(bytes, "image/gif".into(), DEFAULT_IMAGE_LIMITS);
        let released = Arc::downgrade(&images.state);
        drop(images);
        assert!(futures::executor::block_on(pending).unwrap().is_some());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while released.upgrade().is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "image worker retained its owner"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
