//! Typed hook member selection and ordered invocation. A selector can bind a
//! member to its containing Arc, the native counterpart of JS `this` binding.
use super::runtime::TaskDriver;
use crate::{
    storage::{StorageFuture, line::eager},
    *,
};
use futures::FutureExt;
use std::{future::Future, sync::Arc};

pub struct HookKey<H, F> {
    name: JsonString,
    select: Arc<dyn Fn(Arc<H>) -> Result<Option<F>> + Send + Sync>,
}
impl<H, F> Clone for HookKey<H, F> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            select: self.select.clone(),
        }
    }
}
impl<H, F> HookKey<H, F> {
    pub fn new(
        name: impl Into<JsonString>,
        select: impl Fn(Arc<H>) -> Option<F> + Send + Sync + 'static,
    ) -> Self {
        Self::try_new(name, move |handlers| Ok(select(handlers)))
    }
    pub fn try_new(
        name: impl Into<JsonString>,
        select: impl Fn(Arc<H>) -> Result<Option<F>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            select: Arc::new(select),
        }
    }
    pub fn name(&self) -> &JsonString {
        &self.name
    }
}
pub struct HookRunner<H> {
    driver: Arc<dyn TaskDriver>,
    marker: std::marker::PhantomData<fn() -> H>,
}
impl<H> Clone for HookRunner<H> {
    fn clone(&self) -> Self {
        Self {
            driver: self.driver.clone(),
            marker: std::marker::PhantomData,
        }
    }
}
impl<H: Send + Sync + 'static> HookRunner<H> {
    pub(crate) fn new(driver: Arc<dyn TaskDriver>) -> Self {
        Self {
            driver,
            marker: std::marker::PhantomData,
        }
    }
    pub fn each<F: Send + 'static, Fut: Future<Output = Result<()>> + Send + 'static>(
        &self,
        key: &HookKey<H, F>,
        mut invoke: impl FnMut(F) -> Fut + Send + 'static,
    ) -> StorageFuture<()> {
        let agent = self.driver.agent(&Context::default());
        let driver = self.driver.clone();
        let key = key.clone();
        let task = driver.task_name();
        eager(async move {
            let agent = agent.await?;
            for hook in agent.hooks(&task) {
                let Ok(handlers) = hook.handlers.clone().downcast::<H>() else {
                    continue;
                };
                // Member access occurs before the invocation try/catch in the
                // reference. A failing selector therefore rejects the runner.
                let selected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (key.select)(handlers)
                }))
                .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)))?;
                let Some(handler) = selected else {
                    continue;
                };
                let result = std::panic::AssertUnwindSafe(async { invoke(handler).await })
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)));
                if let Err(error) = result {
                    if driver.signal().aborted() {
                        return Err(error);
                    }
                    driver.report_extension(error, Some(hook.extension.clone()));
                }
            }
            Ok(())
        })
    }
}
