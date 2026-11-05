//! Default auth context (port of `auth/context.ts`).

use std::sync::Arc;

use async_trait::async_trait;

use super::types::AuthContext;

/// Env vars from the process environment; file existence via the filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultAuthContext;

#[async_trait]
impl AuthContext for DefaultAuthContext {
    async fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.trim().is_empty())
    }

    async fn file_exists(&self, path: &str) -> bool {
        let resolved = expand_home(path);
        crate::utils::wasm::fs::metadata(resolved).await.is_ok()
    }
}

/// Expand a leading `~` to the home directory.
pub fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix('~')
        && let Some(home) = dirs::home_dir() {
            return format!("{}{}", home.display(), rest);
        }
    path.to_string()
}

pub fn default_provider_auth_context() -> Arc<dyn AuthContext> {
    Arc::new(DefaultAuthContext)
}
