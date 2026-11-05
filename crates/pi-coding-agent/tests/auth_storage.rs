//! Ports of `test/auth-storage.test.ts`, `test/runtime-credentials.test.ts` and
//! `test/models-store.test.ts`. Tests that mock `proper-lockfile` internals are replaced by tests
//! that hold the real `<file>.lock` directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::FutureExt;
use pi_ai::auth::types::{
    ApiKeyCredential, AuthType, Credential, CredentialInfo, CredentialStore, OAuthCredentials,
};
use pi_ai::models_store::{ModelsStore, ModelsStoreEntry};
use pi_ai::types::{AnyModel, Model};
use pi_coding_agent::core::auth_storage::{
    AuthStorage, AuthStorageBackendExt, AuthStorageData, FileAuthStorageBackend, LockResult,
};
use pi_coding_agent::core::models_store::FileModelsStore;
use pi_coding_agent::core::runtime_credentials::RuntimeCredentials;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn api_key(key: &str) -> Credential {
    Credential::ApiKey(ApiKeyCredential { key: Some(key.to_string()), env: None })
}

fn oauth(access: &str, expires: i64) -> Credential {
    Credential::OAuth(OAuthCredentials {
        refresh: "refresh-token".to_string(),
        access: access.to_string(),
        expires,
        extra: Default::default(),
    })
}

fn info(provider: &str) -> CredentialInfo {
    CredentialInfo { provider_id: provider.to_string(), credential_type: AuthType::ApiKey }
}

struct TempAuth {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl TempAuth {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        Self { _dir: dir, path }
    }
    fn path(&self) -> &str {
        self.path.to_str().unwrap()
    }
    fn write(&self, value: Value) {
        std::fs::write(&self.path, value.to_string()).unwrap();
    }
    fn read(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&self.path).unwrap()).unwrap()
    }
}

fn set(f: impl FnOnce() -> Credential + Send + 'static) -> pi_ai::auth::types::ModifyCredentialFn {
    Box::new(move |_| async move { Ok(Some(f())) }.boxed())
}

#[tokio::test]
async fn reads_and_resolves_stored_api_key_credentials() {
    // SAFETY: the variable name is unique to this test.
    unsafe { std::env::set_var("TEST_AUTH_STORAGE_KEY_RS", "environment-key") };
    let auth = TempAuth::new();
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "$TEST_AUTH_STORAGE_KEY_RS" } }));
    let storage = AuthStorage::create(Some(auth.path()));
    assert_eq!(storage.read("anthropic", None).await.unwrap(), Some(api_key("environment-key")));
}

#[cfg(unix)]
#[tokio::test]
async fn resolves_command_backed_api_key_credentials() {
    let auth = TempAuth::new();
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "!printf 'command-key'" } }));
    let storage = AuthStorage::create(Some(auth.path()));
    assert_eq!(storage.read("anthropic", None).await.unwrap(), Some(api_key("command-key")));
}

#[tokio::test]
async fn returns_oauth_credentials_unchanged() {
    let credential = oauth("access-token", pi_ai::utils::now_ms() + 60_000);
    let mut data = AuthStorageData::new();
    data.insert("anthropic".to_string(), credential.clone());
    let storage = AuthStorage::in_memory(Some(&data));
    assert_eq!(storage.read("anthropic", None).await.unwrap(), Some(credential));
}

#[tokio::test]
async fn credential_scoped_env_takes_precedence_and_remains_inspectable() {
    let auth = TempAuth::new();
    auth.write(json!({
        "anthropic": {
            "type": "api_key",
            "key": "$SCOPED_KEY",
            "env": { "SCOPED_KEY": "scoped-value", "REGION": "test-region" },
        }
    }));
    let storage = AuthStorage::create(Some(auth.path()));
    let Some(Credential::ApiKey(credential)) = storage.read("anthropic", None).await.unwrap() else {
        panic!("expected api key credential");
    };
    assert_eq!(credential.key.as_deref(), Some("scoped-value"));
    let env = credential.env.unwrap();
    assert_eq!(env.get("SCOPED_KEY").map(String::as_str), Some("scoped-value"));
    assert_eq!(env.get("REGION").map(String::as_str), Some("test-region"));
}

#[tokio::test]
async fn coalesced_reads_see_external_edits_across_instances() {
    let auth = TempAuth::new();
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "old" } }));
    let first = AuthStorage::create(Some(auth.path()));
    let second = AuthStorage::create(Some(auth.path()));
    auth.write(json!({
        "anthropic": { "type": "api_key", "key": "new" },
        "openai": { "type": "api_key", "key": "openai-key" },
    }));
    let signal = CancellationToken::new();
    let (anthropic, openai, credentials) = tokio::join!(
        first.read("anthropic", Some(&signal)),
        second.read("openai", Some(&signal)),
        first.list(Some(&signal)),
    );
    assert_eq!(anthropic.unwrap(), Some(api_key("new")));
    assert_eq!(openai.unwrap(), Some(api_key("openai-key")));
    assert_eq!(credentials.unwrap(), vec![info("anthropic"), info("openai")]);
    assert_eq!(second.read("anthropic", None).await.unwrap(), Some(api_key("new")));

    let third = AuthStorage::create(Some(auth.path()));
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "newest" } }));
    let (first_reload, third_reload) = tokio::join!(first.read("anthropic", None), third.read("anthropic", None));
    assert_eq!(first_reload.unwrap(), Some(api_key("newest")));
    assert_eq!(third_reload.unwrap(), Some(api_key("newest")));
}

#[cfg(unix)]
#[tokio::test]
async fn creates_new_auth_files_with_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let auth = TempAuth::new();
    AuthStorage::create(Some(auth.path()));
    assert_eq!(std::fs::metadata(&auth.path).unwrap().permissions().mode() & 0o777, 0o600);
}

#[cfg(unix)]
#[tokio::test]
async fn preserves_the_mode_of_an_existing_auth_file() {
    use std::os::unix::fs::PermissionsExt;
    let auth = TempAuth::new();
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "old" } }));
    std::fs::set_permissions(&auth.path, std::fs::Permissions::from_mode(0o660)).unwrap();
    let storage = AuthStorage::create(Some(auth.path()));
    storage.modify("anthropic", set(|| api_key("new")), None).await.unwrap();
    assert_eq!(std::fs::metadata(&auth.path).unwrap().permissions().mode() & 0o777, 0o660);
}

#[tokio::test]
async fn modify_persists_a_credential_while_preserving_unrelated_external_edits() {
    let auth = TempAuth::new();
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "old" } }));
    let storage = AuthStorage::create(Some(auth.path()));
    auth.write(json!({
        "anthropic": { "type": "api_key", "key": "old" },
        "openai": { "type": "api_key", "key": "external" },
    }));
    storage.modify("anthropic", set(|| api_key("new")), None).await.unwrap();
    assert_eq!(
        auth.read(),
        json!({
            "anthropic": { "type": "api_key", "key": "new" },
            "openai": { "type": "api_key", "key": "external" },
        })
    );
}

#[tokio::test]
async fn modify_with_none_leaves_the_current_credential_unchanged() {
    let auth = TempAuth::new();
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "stored" } }));
    let storage = AuthStorage::create(Some(auth.path()));
    let result = storage
        .modify("anthropic", Box::new(|_| async { Ok(None) }.boxed()), None)
        .await
        .unwrap();
    assert_eq!(result, Some(api_key("stored")));
    assert_eq!(storage.read("anthropic", None).await.unwrap(), Some(api_key("stored")));
}

#[tokio::test]
async fn serializes_concurrent_modifications() {
    let auth = TempAuth::new();
    auth.write(json!({}));
    let first = AuthStorage::create(Some(auth.path()));
    let second = AuthStorage::create(Some(auth.path()));
    let (a, b) = tokio::join!(
        first.modify("anthropic", set(|| api_key("anthropic-key")), None),
        second.modify("openai", set(|| api_key("openai-key")), None),
    );
    a.unwrap();
    b.unwrap();
    let written = auth.read();
    assert_eq!(written["anthropic"], json!({ "type": "api_key", "key": "anthropic-key" }));
    assert_eq!(written["openai"], json!({ "type": "api_key", "key": "openai-key" }));
}

#[tokio::test]
async fn delete_removes_one_credential_while_preserving_others() {
    let auth = TempAuth::new();
    auth.write(json!({
        "anthropic": { "type": "api_key", "key": "anthropic-key" },
        "openai": { "type": "api_key", "key": "openai-key" },
    }));
    let storage = AuthStorage::create(Some(auth.path()));
    auth.write(json!({
        "anthropic": { "type": "api_key", "key": "anthropic-key" },
        "openai": { "type": "api_key", "key": "openai-key" },
        "google": { "type": "api_key", "key": "external-key" },
    }));
    storage.delete("anthropic", None).await.unwrap();
    assert_eq!(storage.list(None).await.unwrap(), vec![info("openai"), info("google")]);
    assert_eq!(storage.read("anthropic", None).await.unwrap(), None);
    assert_eq!(storage.read("openai", None).await.unwrap(), Some(api_key("openai-key")));
    assert_eq!(storage.read("google", None).await.unwrap(), Some(api_key("external-key")));
}

#[tokio::test]
async fn in_memory_storage_implements_the_same_credential_store_behavior() {
    let mut data = AuthStorageData::new();
    data.insert("anthropic".to_string(), api_key("initial"));
    let storage = AuthStorage::in_memory(Some(&data));
    assert_eq!(storage.read("anthropic", None).await.unwrap(), Some(api_key("initial")));
    storage.modify("anthropic", set(|| api_key("updated")), None).await.unwrap();
    assert_eq!(storage.read("anthropic", None).await.unwrap(), Some(api_key("updated")));
    storage.delete("anthropic", None).await.unwrap();
    assert_eq!(storage.list(None).await.unwrap(), vec![]);
}

fn hold_lock(path: &Path) -> PathBuf {
    let lock = PathBuf::from(format!("{}.lock", path.display()));
    std::fs::create_dir(&lock).unwrap();
    lock
}

#[tokio::test]
async fn retries_a_briefly_contended_file_lock() {
    let auth = TempAuth::new();
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "stored" } }));
    let lock = hold_lock(&auth.path);
    let backend = FileAuthStorageBackend::new(Some(auth.path()));
    let releaser = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::remove_dir(lock).unwrap();
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    backend
        .with_lock_async(
            move |_| async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(LockResult::read(()))
            },
            None,
        )
        .await
        .unwrap();
    releaser.await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!Path::new(&format!("{}.lock", auth.path())).exists());
}

#[tokio::test]
async fn takes_over_a_stale_lock() {
    let auth = TempAuth::new();
    auth.write(json!({}));
    let lock = hold_lock(&auth.path);
    let old = std::time::SystemTime::now() - Duration::from_secs(120);
    std::fs::File::open(&lock).unwrap().set_modified(old).unwrap();
    let backend = FileAuthStorageBackend::new(Some(auth.path()));
    let value = backend.with_lock(|current| Ok(LockResult::read(current))).unwrap();
    assert_eq!(value.as_deref(), Some("{}"));
}

#[tokio::test]
async fn pre_aborted_file_operations_do_not_create_the_backing_file_or_run_the_mutation() {
    let auth = TempAuth::new();
    let backend = FileAuthStorageBackend::new(Some(auth.path()));
    let signal = CancellationToken::new();
    signal.cancel();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let result = backend
        .with_lock_async(
            move |_| async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(LockResult::write((), "{}".to_string()))
            },
            Some(&signal),
        )
        .await;
    assert!(result.unwrap_err().is::<pi_ai::utils::abort::SignalAbortError>());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!auth.path.exists());
}

#[tokio::test]
async fn aborts_while_waiting_for_a_held_file_lock_without_running_the_mutation_later() {
    let auth = TempAuth::new();
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "stored" } }));
    let lock = hold_lock(&auth.path);
    let backend = FileAuthStorageBackend::new(Some(auth.path()));
    let signal = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let aborter = {
        let signal = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            signal.cancel();
        })
    };
    let result = backend
        .with_lock_async(
            move |_| async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(LockResult::write((), "{}".to_string()))
            },
            Some(&signal),
        )
        .await;
    aborter.await.unwrap();
    assert!(result.unwrap_err().is::<pi_ai::utils::abort::SignalAbortError>());
    std::fs::remove_dir(lock).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(auth.read(), json!({ "anthropic": { "type": "api_key", "key": "stored" } }));
}

#[tokio::test]
async fn holds_the_file_lock_until_a_cancelled_active_callback_settles_without_committing_it() {
    let auth = TempAuth::new();
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "stored" } }));
    let backend = Arc::new(FileAuthStorageBackend::new(Some(auth.path())));
    let signal = CancellationToken::new();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
    let pending = {
        let backend = backend.clone();
        let signal = signal.clone();
        tokio::spawn(async move {
            backend
                .with_lock_async(
                    move |_| async move {
                        let _ = started_tx.send(());
                        let _ = finish_rx.await;
                        Ok(LockResult::write((), json!({ "openai": { "type": "api_key", "key": "cancelled" } }).to_string()))
                    },
                    Some(&signal),
                )
                .await
        })
    };
    started_rx.await.unwrap();
    signal.cancel();
    let competing_calls = Arc::new(AtomicUsize::new(0));
    let competing = {
        let backend = backend.clone();
        let counter = competing_calls.clone();
        tokio::spawn(async move {
            backend
                .with_lock_async(
                    move |_| async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(LockResult::write((), json!({ "google": { "type": "api_key", "key": "committed" } }).to_string()))
                    },
                    None,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(competing_calls.load(Ordering::SeqCst), 0);
    finish_tx.send(()).unwrap();
    assert!(pending.await.unwrap().unwrap_err().is::<pi_ai::utils::abort::SignalAbortError>());
    competing.await.unwrap().unwrap();
    assert_eq!(competing_calls.load(Ordering::SeqCst), 1);
    assert_eq!(auth.read(), json!({ "google": { "type": "api_key", "key": "committed" } }));
}

#[tokio::test]
async fn cancels_a_signalled_credential_read_waiting_for_a_held_file_lock() {
    let auth = TempAuth::new();
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "old" } }));
    let storage = AuthStorage::create(Some(auth.path()));
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "new-value" } }));
    let lock = hold_lock(&auth.path);
    let signal = CancellationToken::new();
    let aborter = {
        let signal = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            signal.cancel();
        })
    };
    let result = storage.read("anthropic", Some(&signal)).await;
    aborter.await.unwrap();
    assert!(result.unwrap_err().is::<pi_ai::utils::abort::SignalAbortError>());
    std::fs::remove_dir(lock).unwrap();
    assert_eq!(storage.read("anthropic", None).await.unwrap(), Some(api_key("new-value")));
}

#[tokio::test]
async fn serializes_in_memory_mutations_across_providers() {
    let storage = AuthStorage::in_memory(None);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
    let first = {
        let storage = storage.clone();
        tokio::spawn(async move {
            storage
                .modify(
                    "anthropic",
                    Box::new(move |_| {
                        async move {
                            let _ = started_tx.send(());
                            let _ = finish_rx.await;
                            Ok(Some(api_key("anthropic-key")))
                        }
                        .boxed()
                    }),
                    None,
                )
                .await
        })
    };
    started_rx.await.unwrap();
    let second_calls = Arc::new(AtomicUsize::new(0));
    let second = {
        let storage = storage.clone();
        let counter = second_calls.clone();
        tokio::spawn(async move {
            storage
                .modify(
                    "openai",
                    Box::new(move |_| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        async { Ok(Some(api_key("openai-key"))) }.boxed()
                    }),
                    None,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    finish_tx.send(()).unwrap();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(storage.read("anthropic", None).await.unwrap(), Some(api_key("anthropic-key")));
    assert_eq!(storage.read("openai", None).await.unwrap(), Some(api_key("openai-key")));
}

#[tokio::test]
async fn cancels_a_queued_in_memory_mutation_without_running_it_later() {
    let storage = AuthStorage::in_memory(None);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
    let first = {
        let storage = storage.clone();
        tokio::spawn(async move {
            storage
                .modify(
                    "anthropic",
                    Box::new(move |_| {
                        async move {
                            let _ = started_tx.send(());
                            let _ = finish_rx.await;
                            Ok(Some(api_key("anthropic-key")))
                        }
                        .boxed()
                    }),
                    None,
                )
                .await
        })
    };
    started_rx.await.unwrap();
    let signal = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let second = {
        let storage = storage.clone();
        let signal = signal.clone();
        tokio::spawn(async move {
            storage
                .modify(
                    "openai",
                    Box::new(move |_| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        async { Ok(Some(api_key("openai-key"))) }.boxed()
                    }),
                    Some(&signal),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    signal.cancel();
    assert!(second.await.unwrap().unwrap_err().is::<pi_ai::utils::abort::SignalAbortError>());
    finish_tx.send(()).unwrap();
    first.await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(storage.read("openai", None).await.unwrap(), None);
}

#[tokio::test]
async fn preserves_the_stored_credential_after_cancelling_an_active_refresh_mutation() {
    let previous = oauth("expired", 0);
    let mut data = AuthStorageData::new();
    data.insert("oauth".to_string(), previous.clone());
    let storage = AuthStorage::in_memory(Some(&data));
    let signal = CancellationToken::new();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
    let pending = {
        let storage = storage.clone();
        let signal = signal.clone();
        tokio::spawn(async move {
            storage
                .modify(
                    "oauth",
                    Box::new(move |_| {
                        async move {
                            let _ = started_tx.send(());
                            let _ = finish_rx.await;
                            Ok(Some(oauth("refreshed", pi_ai::utils::now_ms() + 60_000)))
                        }
                        .boxed()
                    }),
                    Some(&signal),
                )
                .await
        })
    };
    started_rx.await.unwrap();
    signal.cancel();
    assert!(pending.await.unwrap().unwrap_err().is::<pi_ai::utils::abort::SignalAbortError>());
    let calls = Arc::new(AtomicUsize::new(0));
    let competing = {
        let storage = storage.clone();
        let counter = calls.clone();
        tokio::spawn(async move {
            storage
                .modify(
                    "other",
                    Box::new(move |_| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        async { Ok(Some(api_key("other"))) }.boxed()
                    }),
                    None,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    finish_tx.send(()).unwrap();
    competing.await.unwrap().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(storage.read("oauth", None).await.unwrap(), Some(previous));
}

#[tokio::test]
async fn does_not_overwrite_malformed_auth_files() {
    let auth = TempAuth::new();
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "stored" } }));
    let storage = AuthStorage::create(Some(auth.path()));
    std::fs::write(&auth.path, "{invalid-json").unwrap();
    assert!(storage.modify("openai", set(|| api_key("new")), None).await.is_err());
    assert_eq!(std::fs::read_to_string(&auth.path).unwrap(), "{invalid-json");
}

#[tokio::test]
async fn read_stored_credential_reads_without_resolving() {
    let auth = TempAuth::new();
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "$NOT_RESOLVED" } }));
    assert_eq!(
        pi_coding_agent::core::auth_storage::read_stored_credential("anthropic", Some(auth.path())),
        Some(api_key("$NOT_RESOLVED"))
    );
    assert_eq!(pi_coding_agent::core::auth_storage::read_stored_credential("other", Some(auth.path())), None);
}

#[tokio::test]
async fn read_only_storage_validates_and_refuses_writes() {
    use pi_coding_agent::core::auth_storage::ReadOnlyAuthStorage;
    let auth = TempAuth::new();
    auth.write(json!({ "anthropic": { "type": "api_key", "key": "plain" } }));
    let storage = ReadOnlyAuthStorage::new(Some(auth.path()));
    assert_eq!(storage.read("anthropic", None).await.unwrap(), Some(api_key("plain")));
    let error = storage.delete("anthropic", None).await.unwrap_err();
    assert_eq!(error.to_string(), "Read-only credential storage cannot modify auth.json");

    let invalid = TempAuth::new();
    invalid.write(json!({ "anthropic": { "type": "oauth", "access": "a" } }));
    let error = ReadOnlyAuthStorage::new(Some(invalid.path())).list(None).await.unwrap_err();
    assert_eq!(error.to_string(), "Invalid auth.json credential for provider \"anthropic\"");

    let missing = TempAuth::new();
    assert_eq!(ReadOnlyAuthStorage::new(Some(missing.path())).list(None).await.unwrap(), vec![]);
}

// ---------------------------------------------------------------------------
// RuntimeCredentials
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runtime_overrides_mask_stored_credentials_without_persisting() {
    let mut data = AuthStorageData::new();
    data.insert("anthropic".to_string(), api_key("stored-key"));
    let storage = AuthStorage::in_memory(Some(&data));
    let credentials = RuntimeCredentials::new(storage.clone());

    credentials.set_runtime_api_key("anthropic", "runtime-key");
    assert_eq!(credentials.read("anthropic", None).await.unwrap(), Some(api_key("runtime-key")));
    assert_eq!(storage.read("anthropic", None).await.unwrap(), Some(api_key("stored-key")));

    credentials.remove_runtime_api_key("anthropic");
    assert_eq!(credentials.read("anthropic", None).await.unwrap(), Some(api_key("stored-key")));
}

#[tokio::test]
async fn enumeration_merges_overrides_without_exposing_keys() {
    let mut data = AuthStorageData::new();
    data.insert("anthropic".to_string(), oauth("access", pi_ai::utils::now_ms() + 60_000));
    let credentials = RuntimeCredentials::new(AuthStorage::in_memory(Some(&data)));
    credentials.set_runtime_api_key("anthropic", "runtime-key");
    credentials.set_runtime_api_key("openai", "other-runtime-key");
    assert_eq!(credentials.list(None).await.unwrap(), vec![info("anthropic"), info("openai")]);
}

#[tokio::test]
async fn delete_clears_both_the_override_and_persisted_credential() {
    let mut data = AuthStorageData::new();
    data.insert("anthropic".to_string(), api_key("stored-key"));
    let credentials = RuntimeCredentials::new(AuthStorage::in_memory(Some(&data)));
    credentials.set_runtime_api_key("anthropic", "runtime-key");
    credentials.delete("anthropic", None).await.unwrap();
    assert_eq!(credentials.read("anthropic", None).await.unwrap(), None);
    assert_eq!(credentials.list(None).await.unwrap(), vec![]);
}

// ---------------------------------------------------------------------------
// FileModelsStore
// ---------------------------------------------------------------------------

fn model(provider: &str, id: &str) -> AnyModel {
    let model: Model = serde_json::from_value(json!({
        "id": id,
        "name": id,
        "api": "openai-completions",
        "provider": provider,
        "baseUrl": "https://example.test/v1",
        "reasoning": false,
        "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000,
        "maxTokens": 100,
    }))
    .unwrap();
    AnyModel::Chat(model)
}

fn ids(entry: Option<ModelsStoreEntry>) -> Vec<String> {
    entry.unwrap().models.iter().map(|m| m.id().to_string()).collect()
}

#[tokio::test]
async fn persists_provider_catalogs_without_replacing_unrelated_providers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models-store.json");
    let path = path.to_str().unwrap();
    let store = FileModelsStore::new(Some(path));
    store
        .write("one", ModelsStoreEntry { models: vec![model("one", "m1")], checked_at: Some(100), ..Default::default() }, None)
        .await
        .unwrap();
    store
        .write("two", ModelsStoreEntry { models: vec![model("two", "m2")], checked_at: Some(200), ..Default::default() }, None)
        .await
        .unwrap();

    let reloaded = FileModelsStore::new(Some(path));
    assert_eq!(ids(reloaded.read("one", None).await.unwrap()), vec!["m1"]);
    assert_eq!(reloaded.read("one", None).await.unwrap().unwrap().checked_at, Some(100));
    assert_eq!(ids(reloaded.read("two", None).await.unwrap()), vec!["m2"]);

    reloaded.delete("one", None).await.unwrap();
    assert!(reloaded.read("one", None).await.unwrap().is_none());
    assert_eq!(ids(reloaded.read("two", None).await.unwrap()), vec!["m2"]);
}

#[tokio::test]
async fn models_store_sees_external_edits_and_skips_unknown_model_types() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models-store.json");
    std::fs::write(
        &path,
        json!({
            "one": { "models": [serde_json::to_value(model("one", "old")).unwrap(), { "id": "x", "type": "video", "provider": "one" }] },
        })
        .to_string(),
    )
    .unwrap();
    let path_str = path.to_str().unwrap();
    let first = FileModelsStore::new(Some(path_str));
    let second = FileModelsStore::new(Some(path_str));
    let (one, missing) = tokio::join!(first.read("one", None), second.read("missing", None));
    assert_eq!(ids(one.unwrap()), vec!["old"]);
    assert!(missing.unwrap().is_none());

    tokio::time::sleep(Duration::from_millis(5)).await;
    std::fs::write(&path, json!({ "one": { "models": [serde_json::to_value(model("one", "newest-model")).unwrap()] } }).to_string())
        .unwrap();
    assert_eq!(ids(first.read("one", None).await.unwrap()), vec!["newest-model"]);
}

#[tokio::test]
async fn cancels_a_catalog_write_waiting_for_a_held_file_lock_without_writing_later() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models-store.json");
    std::fs::write(&path, "{}").unwrap();
    let lock = hold_lock(&path);
    let store = FileModelsStore::new(Some(path.to_str().unwrap()));
    let signal = CancellationToken::new();
    let aborter = {
        let signal = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            signal.cancel();
        })
    };
    let result = store
        .write("one", ModelsStoreEntry { models: vec![model("one", "m1")], ..Default::default() }, Some(&signal))
        .await;
    aborter.await.unwrap();
    assert!(result.is_err());
    std::fs::remove_dir(lock).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
}
