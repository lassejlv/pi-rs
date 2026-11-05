//! The process-wide default document retention (upstream `setDefaultDocumentRetention`). It is global
//! state, so everything runs in one test of its own binary.
use pi_ai::models::Models;
use pi_durable::{
    SessionOptions, default_document_retention,
    harness::settings::{HarnessSettings, resolve_settings},
    harness::{generation::GenerationPromptInput, registry::*, tool::NativeTool},
    set_default_document_retention, *,
};

#[test]
fn unset_retention_follows_the_process_default_and_explicit_values_win() {
    assert_eq!(default_document_retention(), 600_000.0);
    assert_eq!(SessionOptions::default().document_retention_ms, 600_000.0);
    assert_eq!(
        resolve_settings(HarnessSettings::default()).document_retention_ms,
        600_000.0
    );

    set_default_document_retention(0.0);
    assert_eq!(default_document_retention(), 0.0);
    assert_eq!(SessionOptions::default().document_retention_ms, 0.0);
    assert_eq!(
        resolve_settings(HarnessSettings::default()).document_retention_ms,
        0.0
    );
    // Anything that is not milliseconds of 0 or more means the default, which is now 0.
    let invalid = HarnessSettings {
        document_retention_ms: Some(f64::NAN),
        ..Default::default()
    };
    assert_eq!(resolve_settings(invalid).document_retention_ms, 0.0);
    // Explicit values override it, including Infinity (keep documents).
    for value in [1500.0, f64::INFINITY] {
        let explicit = HarnessSettings {
            document_retention_ms: Some(value),
            ..Default::default()
        };
        assert_eq!(resolve_settings(explicit).document_retention_ms, value);
    }
    let session = SessionOptions {
        document_retention_ms: 42.0,
        ..SessionOptions::default()
    };
    assert_eq!(session.document_retention_ms, 42.0);

    set_default_document_retention(f64::INFINITY);
    assert_eq!(
        SessionOptions::default().document_retention_ms,
        f64::INFINITY
    );
    // A value that is not a number of milliseconds restores the built-in ten minutes.
    set_default_document_retention(-5.0);
    assert_eq!(default_document_retention(), 600_000.0);
    set_default_document_retention(f64::NAN);
    assert_eq!(default_document_retention(), 600_000.0);
    assert_eq!(SessionOptions::default().document_retention_ms, 600_000.0);
    // Global state: the Harness case runs here, not as a parallel test.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(the_harness_reads_the_default_when_it_opens());
}

struct Section(JsonString);
impl RegisteredSection for Section {
    fn key(&self) -> &JsonString {
        &self.0
    }
}
impl PromptSection<GenerationPromptInput<NativeTool, Section>> for Section {
    fn render(
        &self,
        _: std::sync::Arc<GenerationPromptInput<NativeTool, Section>>,
        _: &Context,
    ) -> storage::StorageFuture<Option<JsonString>> {
        Box::pin(async { Ok(None) })
    }
}
fn options() -> HarnessOptions {
    HarnessOptions::new(
        create_registry::<NativeTool, Section>().unwrap(),
        Models::default(),
    )
}
/// Documents the root conversation loaded and left idle, once the Harness is open.
async fn idle_documents(options: HarnessOptions) -> usize {
    let context = Context::default();
    let harness = Harness::open(std::sync::Arc::new(MemoryStorage::new()), options, &context)
        .await
        .unwrap();
    harness
        .root(ConversationOptions::default(), &context)
        .await
        .unwrap();
    let cached = harness.scheduler().session().cache_sizes().cached;
    harness.close(&context).await.unwrap();
    cached
}

/// Upstream reads the process default when the Harness opens, not when its options are built.
async fn the_harness_reads_the_default_when_it_opens() {
    set_default_document_retention(600_000.0);
    let built_early = options();
    let built_early_explicit = {
        let mut explicit = options();
        explicit.default_document_retention_ms = 600_000.0;
        explicit
    };
    set_default_document_retention(0.0);
    // Built while the default was ten minutes, opened while it is 0: documents drop right after use.
    let dropped = idle_documents(built_early).await;
    // An explicit value on the options still wins over the process default.
    let kept = idle_documents(built_early_explicit).await;
    set_default_document_retention(600_000.0);
    assert_eq!(dropped, 0, "the open-time default of 0 applies");
    assert!(kept > 0, "an explicit option overrides the process default");
}
