//! Port of `packages/coding-agent/src/core/auth-storage.ts`.
//!
//! CredentialStore implementation backed by auth.json.
//! Provider auth orchestration belongs to ModelRuntime and pi-ai Models.
//!
//! File locking follows `proper-lockfile` (used by the TS version) so the Rust and TS binaries can
//! share one auth.json: the lock is the directory `<file>.lock`, created with `mkdir`; a lock whose
//! mtime is older than the stale threshold is taken over; holders of async locks refresh the
//! mtime periodically.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_ai::auth::types::{
    ApiKeyCredential, AuthType, Credential, CredentialInfo, CredentialStore, ModifyCredentialFn,
};
use pi_ai::utils::abort::race_with_abort_signal;
use pi_ai::utils::abort::SignalAbortError;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::get_agent_dir;
use crate::core::resolve_config_value::{is_command_config_value, resolve_config_value};
use crate::utils::paths::{get_file_revision, normalize_path};
use crate::utils::text::strip_bom;

/// Credentials keyed by provider id (the shape of auth.json).
pub type AuthStorageData = IndexMap<String, Credential>;

/// Raw auth.json content. Entries are kept as JSON so unknown credential shapes survive rewrites.
type RawAuthData = IndexMap<String, Value>;

/// Result of a locked read-modify-write: `next` replaces the stored content when set.
#[derive(Debug, Clone)]
pub struct LockResult<T> {
    pub result: T,
    pub next: Option<String>,
}

impl<T> LockResult<T> {
    pub fn read(result: T) -> Self {
        Self { result, next: None }
    }
    pub fn write(result: T, next: String) -> Self {
        Self { result, next: Some(next) }
    }
}

/// Synchronous locked callback: receives the current content, returns the next content.
pub type LockFn<'a> = Box<dyn FnOnce(Option<String>) -> anyhow::Result<Option<String>> + Send + 'a>;

/// Asynchronous locked callback: receives the current content, returns the next content.
pub type AsyncLockFn =
    Box<dyn FnOnce(Option<String>) -> BoxFuture<'static, anyhow::Result<Option<String>>> + Send>;

/// Locked storage of one JSON document (auth.json, models-store.json, mcp-auth.json).
///
/// Implementations provide the untyped `*_raw` methods; callers use the typed `with_lock` /
/// `with_lock_async` from [`AuthStorageBackendExt`].
#[async_trait]
pub trait AuthStorageBackend: Send + Sync {
    fn with_lock_raw(&self, f: LockFn<'_>) -> anyhow::Result<()>;
    async fn with_lock_async_raw(
        &self,
        f: AsyncLockFn,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<()>;
}

/// Typed `withLock` / `withLockAsync` over any [`AuthStorageBackend`].
#[async_trait]
pub trait AuthStorageBackendExt: AuthStorageBackend {
    fn with_lock<T: Send>(
        &self,
        f: impl FnOnce(Option<String>) -> anyhow::Result<LockResult<T>> + Send,
    ) -> anyhow::Result<T> {
        let mut out: Option<T> = None;
        let slot = &mut out;
        self.with_lock_raw(Box::new(move |current| {
            let LockResult { result, next } = f(current)?;
            *slot = Some(result);
            Ok(next)
        }))?;
        out.ok_or_else(|| anyhow::anyhow!("Auth storage lock callback did not run"))
    }

    async fn with_lock_async<T, F, Fut>(
        &self,
        f: F,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Option<String>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<LockResult<T>>> + Send + 'static,
    {
        let out: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
        let slot = out.clone();
        self.with_lock_async_raw(
            Box::new(move |current| {
                async move {
                    let LockResult { result, next } = f(current).await?;
                    *slot.lock() = Some(result);
                    Ok(next)
                }
                .boxed()
            }),
            signal,
        )
        .await?;
        let result = out.lock().take();
        result.ok_or_else(|| anyhow::anyhow!("Auth storage lock callback did not run"))
    }
}

impl<B: AuthStorageBackend + ?Sized> AuthStorageBackendExt for B {}

fn throw_if_aborted(signal: Option<&CancellationToken>) -> anyhow::Result<()> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(SignalAbortError.into());
    }
    Ok(())
}

fn default_auth_path() -> String {
    Path::new(&get_agent_dir())
        .join("auth.json")
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// proper-lockfile compatible directory locks
// ---------------------------------------------------------------------------

/// Lock acquisition failure. `ELOCKED` is retryable; `EPERM` is retryable on Windows only.
#[derive(Debug, thiserror::Error)]
enum LockError {
    #[error("Lock file is already being held")]
    Locked,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl LockError {
    fn retryable(&self) -> bool {
        match self {
            LockError::Locked => true,
            LockError::Io(error) => {
                cfg!(windows) && error.kind() == std::io::ErrorKind::PermissionDenied
            }
        }
    }
    fn is_eperm(&self) -> bool {
        matches!(self, LockError::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied)
    }
}

fn lock_dir_path(path: &str) -> PathBuf {
    PathBuf::from(format!("{path}.lock"))
}

fn is_lock_stale(lock_path: &Path, stale: Duration) -> std::io::Result<bool> {
    let modified = std::fs::metadata(lock_path)?.modified()?;
    Ok(SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > stale))
}

fn try_acquire_lock(path: &str, stale: Duration) -> Result<PathBuf, LockError> {
    let lock_path = lock_dir_path(path);
    let mut allow_stale_takeover = true;
    loop {
        match std::fs::create_dir(&lock_path) {
            Ok(()) => return Ok(lock_path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !allow_stale_takeover {
                    return Err(LockError::Locked);
                }
                match is_lock_stale(&lock_path, stale) {
                    // The holder released between mkdir and stat: retry right away.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                    Ok(false) => return Err(LockError::Locked),
                    Ok(true) => {
                        match std::fs::remove_dir(&lock_path) {
                            Ok(()) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                        }
                        allow_stale_takeover = false;
                    }
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn touch_lock(lock_path: &Path) -> std::io::Result<()> {
    let file = std::fs::File::open(lock_path)?;
    file.set_modified(SystemTime::now())
}

/// A held lock. Dropping it releases the lock.
struct HeldLock {
    lock_path: PathBuf,
    updater: Option<CancellationToken>,
    released: bool,
}

impl HeldLock {
    fn release(&mut self) -> std::io::Result<()> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        if let Some(updater) = self.updater.take() {
            updater.cancel();
        }
        match std::fs::remove_dir(&self.lock_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

/// Shared compromise flag of an async lock (`onCompromised`).
#[derive(Default)]
struct Compromise {
    compromised: AtomicBool,
    message: Mutex<Option<String>>,
}

impl Compromise {
    fn check(&self) -> anyhow::Result<()> {
        if self.compromised.load(Ordering::SeqCst) {
            let message = self
                .message
                .lock()
                .clone()
                .unwrap_or_else(|| "Auth storage lock was compromised".to_string());
            anyhow::bail!(message);
        }
        Ok(())
    }
}

/// Keep an async lock fresh: refresh its mtime every `stale / 2`, and flag the lock as compromised
/// when it disappears or the refresh comes too late.
fn spawn_lock_updater(
    lock_path: PathBuf,
    stale: Duration,
    compromise: Arc<Compromise>,
) -> CancellationToken {
    let token = CancellationToken::new();
    let stop = token.clone();
    crate::utils::uncaught::spawn(async move {
        let interval = stale / 2;
        let mut last = Instant::now();
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(interval) => {}
            }
            let late = last.elapsed() > stale;
            let failure = match touch_lock(&lock_path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Some("Lock is no longer held: the lock directory was removed".to_string())
                }
                _ if late => Some("Unable to update lock within the stale threshold".to_string()),
                _ => None,
            };
            if let Some(message) = failure {
                *compromise.message.lock() = Some(message);
                compromise.compromised.store(true, Ordering::SeqCst);
                return;
            }
            last = Instant::now();
        }
    });
    token
}

// On Windows, a just-removed lock directory can stay "delete pending" while another process
// (antivirus, indexer, another pi checking the lock) still has it open. mkdir then fails with
// EPERM until that handle closes. Retry this briefly so real permission errors still fail fast.
const MAX_EPERM_RETRIES: u32 = 5;
const SYNC_LOCK_STALE: Duration = Duration::from_millis(10_000);
const ASYNC_LOCK_STALE: Duration = Duration::from_millis(30_000);

fn open_with_mode(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // The mode applies only on creation so administrator-managed modes and ACLs remain intact.
        options.mode(0o600);
    }
    options.open(path)
}

fn write_auth_file(path: &str, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = open_with_mode(Path::new(path))?;
    file.write_all(content.as_bytes())
}

fn read_if_exists(path: &str) -> anyhow::Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// TS: `implements AuthStorageBackend`
pub struct FileAuthStorageBackend {
    auth_path: String,
}

impl FileAuthStorageBackend {
    pub fn new(auth_path: Option<&str>) -> Self {
        let path = auth_path.map(str::to_string).unwrap_or_else(default_auth_path);
        Self {
            auth_path: normalize_path(&path, None),
        }
    }

    pub fn path(&self) -> &str {
        &self.auth_path
    }

    fn ensure_parent_dir(&self) -> std::io::Result<()> {
        let Some(dir) = Path::new(&self.auth_path).parent() else {
            return Ok(());
        };
        if dir.as_os_str().is_empty() || dir.exists() {
            return Ok(());
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(dir)
    }

    fn ensure_file_exists(&self) -> std::io::Result<()> {
        if !Path::new(&self.auth_path).exists() {
            write_auth_file(&self.auth_path, "{}")?;
        }
        Ok(())
    }

    fn acquire_lock_sync_with_retry(&self) -> anyhow::Result<HeldLock> {
        const MAX_ATTEMPTS: u32 = 10;
        const DELAY: Duration = Duration::from_millis(20);
        let mut attempt = 1;
        loop {
            match try_acquire_lock(&self.auth_path, SYNC_LOCK_STALE) {
                Ok(lock_path) => {
                    return Ok(HeldLock {
                        lock_path,
                        updater: None,
                        released: false,
                    });
                }
                Err(error) => {
                    if !error.retryable() || attempt == MAX_ATTEMPTS {
                        return Err(error.into());
                    }
                    std::thread::sleep(DELAY);
                    attempt += 1;
                }
            }
        }
    }

    async fn acquire_lock_async(
        &self,
        signal: Option<&CancellationToken>,
        compromise: Arc<Compromise>,
    ) -> anyhow::Result<HeldLock> {
        const MAX_DELAY_MS: u64 = 2_000;
        let deadline = Instant::now() + ASYNC_LOCK_STALE;
        let mut retry: u32 = 0;
        let mut eperm_retries = 0;
        loop {
            throw_if_aborted(signal)?;
            match try_acquire_lock(&self.auth_path, ASYNC_LOCK_STALE) {
                Ok(lock_path) => {
                    let updater =
                        spawn_lock_updater(lock_path.clone(), ASYNC_LOCK_STALE, compromise);
                    let mut lock = HeldLock {
                        lock_path,
                        updater: Some(updater),
                        released: false,
                    };
                    if signal.is_some_and(CancellationToken::is_cancelled) {
                        let _ = lock.release();
                        return Err(SignalAbortError.into());
                    }
                    return Ok(lock);
                }
                Err(error) => {
                    throw_if_aborted(signal)?;
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if !error.retryable() || remaining.is_zero() {
                        return Err(error.into());
                    }
                    // The 30s deadline is for waiting on another holder; cap EPERM retries separately.
                    if error.is_eperm() {
                        eperm_retries += 1;
                        if eperm_retries > MAX_EPERM_RETRIES {
                            return Err(error.into());
                        }
                    }
                    let base_delay_ms = (10u64 << retry.min(16)).min(MAX_DELAY_MS / 2);
                    retry += 1;
                    let jitter: f64 = rand_unit();
                    let delay_ms = ((base_delay_ms as f64) * (1.0 + jitter)).round() as u64;
                    let delay = Duration::from_millis(delay_ms).min(remaining);
                    match signal {
                        Some(signal) => {
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => {}
                                _ = signal.cancelled() => return Err(SignalAbortError.into()),
                            }
                        }
                        None => tokio::time::sleep(delay).await,
                    }
                }
            }
        }
    }
}

/// Uniform random number in [0, 1) without a `rand` dependency.
fn rand_unit() -> f64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos ^ COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

#[async_trait]
impl AuthStorageBackend for FileAuthStorageBackend {
    fn with_lock_raw(&self, f: LockFn<'_>) -> anyhow::Result<()> {
        self.ensure_parent_dir()?;
        self.ensure_file_exists()?;
        let mut lock = self.acquire_lock_sync_with_retry()?;
        let result = (|| {
            let current = read_if_exists(&self.auth_path)?;
            if let Some(next) = f(current)? {
                write_auth_file(&self.auth_path, &next)?;
            }
            Ok(())
        })();
        let _ = lock.release();
        result
    }

    async fn with_lock_async_raw(
        &self,
        f: AsyncLockFn,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<()> {
        throw_if_aborted(signal)?;
        self.ensure_parent_dir()?;
        self.ensure_file_exists()?;

        let compromise = Arc::new(Compromise::default());
        let mut lock = self.acquire_lock_async(signal, compromise.clone()).await?;
        let result = async {
            compromise.check()?;
            throw_if_aborted(signal)?;
            let current = read_if_exists(&self.auth_path)?;
            let next = f(current).await?;
            compromise.check()?;
            throw_if_aborted(signal)?;
            if let Some(next) = next {
                write_auth_file(&self.auth_path, &next)?;
            }
            compromise.check()?;
            Ok(())
        }
        .await;
        // Ignore unlock errors when the lock is compromised.
        let _ = lock.release();
        result
    }
}

// ---------------------------------------------------------------------------
// ReadOnlyAuthStorage
// ---------------------------------------------------------------------------

fn validate_auth_data(parsed: &Value) -> anyhow::Result<RawAuthData> {
    let Value::Object(entries) = parsed else {
        anyhow::bail!("Invalid auth.json: expected an object");
    };
    for (provider_id, credential) in entries {
        let invalid = || anyhow::anyhow!("Invalid auth.json credential for provider \"{provider_id}\"");
        let Value::Object(value) = credential else {
            return Err(invalid());
        };
        match value.get("type").and_then(Value::as_str) {
            Some("api_key") => {
                let valid_key = value.get("key").is_none_or(Value::is_string);
                let valid_env = value.get("env").is_none_or(|env| {
                    env.as_object()
                        .is_some_and(|env| env.values().all(Value::is_string))
                });
                if valid_key && valid_env {
                    continue;
                }
            }
            Some("oauth")
                if value.get("access").is_some_and(Value::is_string)
                    && value.get("refresh").is_some_and(Value::is_string)
                    && value
                        .get("expires")
                        .and_then(Value::as_f64)
                        .is_some_and(f64::is_finite) =>
            {
                continue;
            }
            _ => {}
        }
        return Err(invalid());
    }
    Ok(entries.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

fn credential_from_value(value: &Value) -> Option<Credential> {
    serde_json::from_value(value.clone()).ok()
}

fn credential_type(value: &Value) -> Option<AuthType> {
    match value.get("type").and_then(Value::as_str) {
        Some("api_key") => Some(AuthType::ApiKey),
        Some("oauth") => Some(AuthType::OAuth),
        _ => None,
    }
}

fn env_to_index_map(
    env: Option<&pi_ai::types::ProviderEnv>,
) -> Option<IndexMap<String, String>> {
    env.map(|env| env.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// Resolve a stored api-key credential's configured key value.
fn resolve_api_key_credential(credential: &ApiKeyCredential) -> ApiKeyCredential {
    let key = credential
        .key
        .as_deref()
        .and_then(|key| resolve_config_value(key, env_to_index_map(credential.env.as_ref())));
    ApiKeyCredential {
        key,
        env: credential.env.clone(),
    }
}

/// Read-only view of auth.json for processes that must never write credentials.
/// TS: `implements CredentialStore`
pub struct ReadOnlyAuthStorage {
    auth_path: String,
    data: Mutex<Option<RawAuthData>>,
}

impl ReadOnlyAuthStorage {
    pub fn new(auth_path: Option<&str>) -> Self {
        let path = auth_path.map(str::to_string).unwrap_or_else(default_auth_path);
        Self {
            auth_path: normalize_path(&path, None),
            data: Mutex::new(None),
        }
    }

    fn load(&self) -> anyhow::Result<RawAuthData> {
        if let Some(data) = self.data.lock().clone() {
            return Ok(data);
        }
        let content = match std::fs::read(&self.auth_path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                *self.data.lock() = Some(RawAuthData::new());
                return Ok(RawAuthData::new());
            }
            Err(error) => anyhow::bail!("Failed to read auth.json: {error}"),
        };
        let parsed: Value = serde_json::from_str(&strip_bom(&content))
            .map_err(|error| anyhow::anyhow!("Failed to read auth.json: {error}"))?;
        let data = validate_auth_data(&parsed)?;
        *self.data.lock() = Some(data.clone());
        Ok(data)
    }
}

#[async_trait]
impl CredentialStore for ReadOnlyAuthStorage {
    async fn read(
        &self,
        provider_id: &str,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<Option<Credential>> {
        throw_if_aborted(signal)?;
        let data = self.load()?;
        throw_if_aborted(signal)?;
        let Some(credential) = data.get(provider_id).and_then(credential_from_value) else {
            return Ok(None);
        };
        match &credential {
            Credential::ApiKey(api_key)
                if api_key
                    .key
                    .as_deref()
                    .is_some_and(|key| !key.is_empty() && !is_command_config_value(key)) =>
            {
                Ok(Some(Credential::ApiKey(resolve_api_key_credential(api_key))))
            }
            _ => Ok(Some(credential)),
        }
    }

    async fn list(&self, signal: Option<&CancellationToken>) -> anyhow::Result<Vec<CredentialInfo>> {
        throw_if_aborted(signal)?;
        let credentials = self
            .load()?
            .iter()
            .filter_map(|(provider_id, credential)| {
                Some(CredentialInfo {
                    provider_id: provider_id.clone(),
                    credential_type: credential_type(credential)?,
                })
            })
            .collect();
        throw_if_aborted(signal)?;
        Ok(credentials)
    }

    async fn modify(
        &self,
        _provider_id: &str,
        _f: ModifyCredentialFn,
        _signal: Option<&CancellationToken>,
    ) -> anyhow::Result<Option<Credential>> {
        anyhow::bail!("Read-only credential storage cannot modify auth.json")
    }

    async fn delete(&self, _provider_id: &str, _signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        anyhow::bail!("Read-only credential storage cannot modify auth.json")
    }
}

// ---------------------------------------------------------------------------
// InMemoryAuthStorageBackend
// ---------------------------------------------------------------------------

/// TS: `implements AuthStorageBackend`
#[derive(Default)]
pub struct InMemoryAuthStorageBackend {
    value: Arc<Mutex<Option<String>>>,
    chain: Arc<tokio::sync::Mutex<()>>,
}

impl InMemoryAuthStorageBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl AuthStorageBackend for InMemoryAuthStorageBackend {
    fn with_lock_raw(&self, f: LockFn<'_>) -> anyhow::Result<()> {
        let current = self.value.lock().clone();
        if let Some(next) = f(current)? {
            *self.value.lock() = Some(next);
        }
        Ok(())
    }

    async fn with_lock_async_raw(
        &self,
        f: AsyncLockFn,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<()> {
        // Operations run in call order. An aborted caller stops waiting, but an operation that
        // already started runs to completion (it re-checks the signal before writing).
        let guard = match signal {
            Some(signal) => {
                race_with_abort_signal(async { Ok(self.chain.clone().lock_owned().await) }, signal)
                    .await?
            }
            None => self.chain.clone().lock_owned().await,
        };
        let value = self.value.clone();
        let task_signal = signal.cloned();
        let operation = tokio::spawn(async move {
            let _guard = guard;
            throw_if_aborted(task_signal.as_ref())?;
            let current = value.lock().clone();
            let next = f(current).await?;
            throw_if_aborted(task_signal.as_ref())?;
            if let Some(next) = next {
                *value.lock() = Some(next);
            }
            anyhow::Ok(())
        });
        let joined = async {
            operation
                .await
                .map_err(|error| anyhow::anyhow!("Auth storage operation failed: {error}"))?
        };
        match signal {
            Some(signal) => race_with_abort_signal(joined, signal).await,
            None => joined.await,
        }
    }
}

// ---------------------------------------------------------------------------
// AuthStorage
// ---------------------------------------------------------------------------

type ReloadFuture = Shared<BoxFuture<'static, Result<RawAuthData, Arc<anyhow::Error>>>>;

struct AuthFileReload {
    id: u64,
    controller: CancellationToken,
    promise: ReloadFuture,
    readers: usize,
}

#[derive(Default)]
struct AuthFileReadState {
    data: RawAuthData,
    revision: Option<String>,
    reload: Option<AuthFileReload>,
}

type SharedReadState = Arc<Mutex<AuthFileReadState>>;

/// The read state of the first file-backed store, shared by later stores of the same path.
static SHARED_AUTH_FILE_READ_STATE: Mutex<Option<(String, SharedReadState)>> = Mutex::new(None);
static NEXT_RELOAD_ID: AtomicU64 = AtomicU64::new(1);

fn parse_storage_data(content: Option<&str>) -> anyhow::Result<RawAuthData> {
    match content {
        None | Some("") => Ok(RawAuthData::new()),
        Some(content) => {
            let parsed: Value = crate::utils::js_json::js_json_parse(&strip_bom(content)).map_err(anyhow::Error::msg)?;
            match parsed {
                Value::Object(map) => Ok(map.into_iter().collect()),
                Value::Null => Ok(RawAuthData::new()),
                _ => anyhow::bail!("Invalid auth.json: expected an object"),
            }
        }
    }
}

fn stringify_pretty(data: &RawAuthData) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string())
}

/// Credential storage backed by a JSON file.
/// TS: `implements CredentialStore`
pub struct AuthStorage {
    storage: Arc<dyn AuthStorageBackend>,
    auth_path: Option<String>,
    read_state: SharedReadState,
}

impl AuthStorage {
    pub fn new(storage: Arc<dyn AuthStorageBackend>, auth_path: Option<&str>) -> Self {
        let auth_path = auth_path.map(str::to_string);
        let read_state = {
            let mut shared = SHARED_AUTH_FILE_READ_STATE.lock();
            match (&auth_path, shared.as_ref()) {
                (Some(path), Some((shared_path, state))) if shared_path == path => state.clone(),
                (Some(path), None) => {
                    let state: SharedReadState = Arc::default();
                    *shared = Some((path.clone(), state.clone()));
                    state
                }
                _ => Arc::default(),
            }
        };
        let storage = Self {
            storage,
            auth_path,
            read_state,
        };
        if let Some(path) = &storage.auth_path {
            let revision = get_file_revision(path);
            if revision.is_some() && revision == storage.read_state.lock().revision {
                return storage;
            }
        }
        storage.reload();
        storage
    }

    pub fn create(auth_path: Option<&str>) -> Arc<AuthStorage> {
        let path = auth_path.map(str::to_string).unwrap_or_else(default_auth_path);
        let normalized = normalize_path(&path, None);
        Arc::new(AuthStorage::new(
            Arc::new(FileAuthStorageBackend::new(Some(&normalized))),
            Some(&normalized),
        ))
    }

    pub fn from_storage(storage: Arc<dyn AuthStorageBackend>) -> Arc<AuthStorage> {
        Arc::new(AuthStorage::new(storage, None))
    }

    pub fn in_memory(data: Option<&AuthStorageData>) -> Arc<AuthStorage> {
        let storage = InMemoryAuthStorageBackend::new();
        let content = serde_json::to_string_pretty(&data.cloned().unwrap_or_default())
            .unwrap_or_else(|_| "{}".to_string());
        let _ = storage.with_lock(move |_| Ok(LockResult::write((), content)));
        AuthStorage::from_storage(Arc::new(storage))
    }

    fn update_read_state(state: &SharedReadState, data: RawAuthData, revision: Option<String>) {
        let mut state = state.lock();
        state.data = data;
        state.revision = revision;
    }

    /// Reload credentials from storage.
    pub fn reload(&self) {
        let auth_path = self.auth_path.clone();
        let result = self.storage.with_lock(|current| {
            let revision = auth_path.as_deref().and_then(get_file_revision);
            Ok(LockResult::read((current, revision)))
        });
        // Preserve the last valid in-memory snapshot on failure.
        if let Ok((content, revision)) = result
            && let Ok(data) = parse_storage_data(content.as_deref())
        {
            Self::update_read_state(&self.read_state, data, revision);
        }
    }

    fn reload_from_storage_async(
        storage: Arc<dyn AuthStorageBackend>,
        auth_path: Option<String>,
        read_state: SharedReadState,
        signal: Option<CancellationToken>,
    ) -> BoxFuture<'static, anyhow::Result<RawAuthData>> {
        async move {
            storage
                .with_lock_async(
                    move |content| async move {
                        let data = parse_storage_data(content.as_deref())?;
                        let revision = auth_path.as_deref().and_then(get_file_revision);
                        AuthStorage::update_read_state(&read_state, data.clone(), revision);
                        Ok(LockResult::read(data))
                    },
                    signal.as_ref(),
                )
                .await
        }
        .boxed()
    }

    async fn read_latest_data(&self, signal: Option<&CancellationToken>) -> anyhow::Result<RawAuthData> {
        throw_if_aborted(signal)?;
        let Some(auth_path) = &self.auth_path else {
            let reload = Self::reload_from_storage_async(
                self.storage.clone(),
                None,
                self.read_state.clone(),
                signal.cloned(),
            )
            .await;
            return match (reload, signal) {
                (Ok(data), _) => Ok(data),
                (Err(error), Some(_)) => Err(error),
                (Err(_), None) => Ok(self.read_state.lock().data.clone()),
            };
        };

        let revision = get_file_revision(auth_path);
        let (id, promise) = {
            let mut state = self.read_state.lock();
            if revision.is_some() && revision == state.revision {
                return Ok(state.data.clone());
            }
            if state.reload.is_none() {
                let controller = CancellationToken::new();
                let promise = Self::reload_from_storage_async(
                    self.storage.clone(),
                    Some(auth_path.clone()),
                    self.read_state.clone(),
                    Some(controller.clone()),
                )
                .map(|result| result.map_err(Arc::new))
                .boxed()
                .shared();
                state.reload = Some(AuthFileReload {
                    id: NEXT_RELOAD_ID.fetch_add(1, Ordering::Relaxed),
                    controller,
                    promise,
                    readers: 0,
                });
            }
            let reload = state.reload.as_mut().expect("reload in flight");
            reload.readers += 1;
            (reload.id, reload.promise.clone())
        };

        let operation = async { promise.await.map_err(|error| anyhow::anyhow!("{error:#}")) };
        let result = match signal {
            Some(signal) => race_with_abort_signal(operation, signal).await,
            None => operation.await,
        };

        {
            let mut state = self.read_state.lock();
            if let Some(reload) = state.reload.as_mut()
                && reload.id == id
            {
                reload.readers -= 1;
                let settled = reload.promise.peek().is_some();
                if reload.readers == 0 || settled {
                    let reload = state.reload.take().expect("reload in flight");
                    if !settled {
                        reload.controller.cancel();
                    }
                }
            }
        }

        match (result, signal) {
            (Ok(data), _) => Ok(data),
            (Err(error), Some(_)) => Err(error),
            (Err(_), None) => Ok(self.read_state.lock().data.clone()),
        }
    }
}

#[async_trait]
impl CredentialStore for AuthStorage {
    async fn read(
        &self,
        provider: &str,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<Option<Credential>> {
        let data = self.read_latest_data(signal).await?;
        throw_if_aborted(signal)?;
        let Some(credential) = data.get(provider).and_then(credential_from_value) else {
            return Ok(None);
        };
        match &credential {
            Credential::ApiKey(api_key) if api_key.key.is_some() => {
                Ok(Some(Credential::ApiKey(resolve_api_key_credential(api_key))))
            }
            _ => Ok(Some(credential)),
        }
    }

    async fn modify(
        &self,
        provider: &str,
        f: ModifyCredentialFn,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<Option<Credential>> {
        let provider_id = provider.to_string();
        let auth_path = self.auth_path.clone();
        let (result, latest, revision) = self
            .storage
            .with_lock_async(
                move |content| async move {
                    let mut current = parse_storage_data(content.as_deref())?;
                    let existing = current.get(&provider_id).and_then(credential_from_value);
                    match f(existing.clone()).await? {
                        None => {
                            let revision = auth_path.as_deref().and_then(get_file_revision);
                            Ok(LockResult::read((existing, current, revision)))
                        }
                        Some(next) => {
                            current.insert(provider_id, serde_json::to_value(&next)?);
                            let content = stringify_pretty(&current);
                            Ok(LockResult::write((Some(next), current, None), content))
                        }
                    }
                },
                signal,
            )
            .await?;
        Self::update_read_state(&self.read_state, latest, revision);
        Ok(result)
    }

    async fn delete(&self, provider: &str, signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        let provider_id = provider.to_string();
        let latest = self
            .storage
            .with_lock_async(
                move |content| async move {
                    let mut current = parse_storage_data(content.as_deref())?;
                    current.shift_remove(&provider_id);
                    let content = stringify_pretty(&current);
                    Ok(LockResult::write(current, content))
                },
                signal,
            )
            .await?;
        Self::update_read_state(&self.read_state, latest, None);
        Ok(())
    }

    /// List credential metadata without resolving configured key values.
    async fn list(&self, signal: Option<&CancellationToken>) -> anyhow::Result<Vec<CredentialInfo>> {
        let data = self.read_latest_data(signal).await?;
        throw_if_aborted(signal)?;
        Ok(data
            .iter()
            .filter_map(|(provider_id, credential)| {
                Some(CredentialInfo {
                    provider_id: provider_id.clone(),
                    credential_type: credential_type(credential)?,
                })
            })
            .collect())
    }
}

/// One-off synchronous read of a stored credential from an auth.json file,
/// without instantiating a store or resolving configured key values.
pub fn read_stored_credential(provider_id: &str, auth_path: Option<&str>) -> Option<Credential> {
    let path = auth_path.map(str::to_string).unwrap_or_else(default_auth_path);
    let content = std::fs::read_to_string(normalize_path(&path, None)).ok()?;
    let data: serde_json::Map<String, Value> = serde_json::from_str(&strip_bom(&content)).ok()?;
    data.get(provider_id).and_then(credential_from_value)
}
