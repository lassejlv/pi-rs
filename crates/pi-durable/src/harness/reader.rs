//! Read-only registry capabilities used by the scheduler. A view retains the
//! full typed snapshot, so a phase can use application registrations without
//! coupling task execution to one application's tool and section types.
use super::{
    agent::{AgentState, AgentView, try_resolve_agent},
    catalog::*,
    registry::*,
    settings::Settings,
};
use crate::*;
use std::{any::Any, sync::Arc};

#[derive(Clone)]
pub struct RegistryView {
    catalog: TaskCatalogSnapshot,
    snapshot: Option<Arc<dyn Any + Send + Sync>>,
    resolve: Option<ResolveAgent>,
}
type ResolveAgent = Arc<
    dyn Fn(
            Option<&AgentState>,
            &Settings,
            &mut dyn FnMut(Error, &JsonString) -> Result<()>,
        ) -> Result<AgentView>
        + Send
        + Sync,
>;
impl RegistryView {
    pub fn from_catalog(catalog: TaskCatalogSnapshot) -> Self {
        Self {
            catalog,
            snapshot: None,
            resolve: None,
        }
    }
    pub fn typed<T: RegisteredTool, S: RegisteredSection>(&self) -> Result<RegistrySnapshot<T, S>> {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.downcast_ref::<RegistrySnapshot<T, S>>())
            .cloned()
            .ok_or_else(|| {
                Error::message("Registry snapshot does not have the requested registration types")
            })
    }
    pub fn task(&self, name: &JsonString) -> Option<&InstalledTask> {
        self.catalog.task(name)
    }
    pub fn tasks(&self) -> impl Iterator<Item = &InstalledTask> {
        self.catalog.tasks()
    }
    pub fn same_snapshot(&self, other: &Self) -> bool {
        self.catalog.same_snapshot(&other.catalog)
    }
    pub fn resolve_agent(
        &self,
        state: Option<&AgentState>,
        settings: &Settings,
        mut report: impl FnMut(Error, &JsonString) -> Result<()>,
    ) -> Result<AgentView> {
        let resolve = self.resolve.as_ref().ok_or_else(|| {
            Error::message("Registry snapshot does not carry extension registrations")
        })?;
        resolve(state, settings, &mut report)
    }
}
impl<T: RegisteredTool, S: RegisteredSection> From<RegistrySnapshot<T, S>> for RegistryView {
    fn from(snapshot: RegistrySnapshot<T, S>) -> Self {
        let resolve = snapshot.clone();
        Self {
            catalog: snapshot.task_catalog(),
            snapshot: Some(Arc::new(snapshot)),
            resolve: Some(Arc::new(move |state, settings, report| {
                try_resolve_agent(state, &resolve, settings, report)
                    .map(|agent| Arc::new(agent).into())
            })),
        }
    }
}

pub struct ReaderSubscription {
    close: Option<Box<dyn FnOnce() -> Result<()> + Send + Sync>>,
}
impl ReaderSubscription {
    pub fn new(close: impl FnOnce() -> Result<()> + Send + Sync + 'static) -> Self {
        Self {
            close: Some(Box::new(close)),
        }
    }
    pub fn close(mut self) -> Result<()> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.close.take().unwrap()()
        }))
        .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)))
    }
}
impl Drop for ReaderSubscription {
    fn drop(&mut self) {
        if let Some(close) = self.close.take() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(close));
        }
    }
}
pub trait RegistryReader: Send + Sync + 'static {
    fn snapshot(&self) -> Result<RegistryView>;
    fn subscribe(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Result<ReaderSubscription>;
}
impl<R: RegistryReader + ?Sized> RegistryReader for Arc<R> {
    fn snapshot(&self) -> Result<RegistryView> {
        self.as_ref().snapshot()
    }
    fn subscribe(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Result<ReaderSubscription> {
        self.as_ref().subscribe(listener)
    }
}
impl RegistryReader for TaskCatalog {
    fn snapshot(&self) -> Result<RegistryView> {
        Ok(RegistryView::from_catalog(self.snapshot()))
    }
    fn subscribe(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Result<ReaderSubscription> {
        let subscription = self.subscribe(move || listener());
        Ok(ReaderSubscription::new(move || {
            drop(subscription);
            Ok(())
        }))
    }
}
impl<T: RegisteredTool, S: RegisteredSection> RegistryReader for Registry<T, S> {
    fn snapshot(&self) -> Result<RegistryView> {
        Ok(self.snapshot().into())
    }
    fn subscribe(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Result<ReaderSubscription> {
        let subscription = self.subscribe(move || listener());
        Ok(ReaderSubscription::new(move || {
            drop(subscription);
            Ok(())
        }))
    }
}
pub(crate) fn read_registry(reader: &dyn RegistryReader) -> Result<RegistryView> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader.snapshot()))
        .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)))
}
