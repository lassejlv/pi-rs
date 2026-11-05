//! Ports of the upstream env-truncate, types (runtime parts), harness-telemetry-options and
//! offline halves of the provider-session-cache and system-order-cache tests.
use parking_lot::Mutex;
use pi_ai::{models::Models, providers::faux::*};
use pi_durable::{
    harness::{
        generation::GenerationPromptInput, provider::provider_doc, registry::*, tool::NativeTool,
    },
    truncate::{TruncationOptions, format_size, truncate_head},
    *,
};
use pi_telemetry::InMemoryTelemetryContext;
use std::sync::Arc;

fn opts(max_bytes: f64, max_lines: f64) -> TruncationOptions {
    TruncationOptions {
        max_bytes: Some(max_bytes),
        max_lines: Some(max_lines),
    }
}
fn utf8_len(text: &str) -> usize {
    text.len()
}

#[test]
fn reports_utf8_byte_counts_in_truncation_results() {
    let content = "aé🙂\nb";
    let result = truncate_head(&content.into(), opts(100.0, 10.0));
    assert!(!result.truncated);
    assert_eq!(result.total_bytes, utf8_len(content) as f64);
    assert_eq!(result.output_bytes, utf8_len(content));
    assert_eq!(result.total_bytes, 9.0);
}

#[test]
fn counts_utf8_bytes_and_truncates_correctly_in_a_runtime_without_buffer() {
    // Rust has no Buffer; the byte counter is the native one. Lone surrogates count as the three-byte
    // replacement character, as TextEncoder encodes them.
    let inputs: [&[u16]; 9] = [
        &[],
        &[0x61, 0x73, 0x63, 0x69, 0x69],
        &[0xe9],
        &[0x4e2d],
        &[0xd83d, 0xde42],
        &[0xd83d],
        &[0xde42],
        &[0x61, 0xd83d, 0xde42, 0x62],
        &[0x07ff, 0x0800, 0xffff],
    ];
    let expected = [0, 5, 2, 3, 4, 3, 3, 6, 8];
    for (units, expected) in inputs.iter().zip(expected) {
        let text = JsonString::from_utf16(units.to_vec());
        assert_eq!(
            pi_chord::output::utf8_byte_length(&text),
            expected,
            "{units:?}"
        );
    }
    let head = truncate_head(&"aé🙂\nb".into(), opts(7.0, 10.0));
    assert_eq!(head.content.to_string_lossy(), "aé🙂");
    assert_eq!(head.output_bytes, 7);
    assert_eq!(head.truncated_by, Some("bytes"));
}

#[test]
fn does_not_count_a_trailing_newline_as_an_extra_line() {
    let content = format!("{}\n", ["line"; 3].join("\n"));
    let head = truncate_head(&content.into(), opts(100.0, 3.0));
    assert!(!head.truncated);
    assert_eq!(head.total_lines, 3.0);
    assert_eq!(head.output_lines, 3);
}

#[test]
fn truncates_head_by_line_limits() {
    let head = truncate_head(&"one\ntwo\nthree\nfour".into(), opts(100.0, 2.0));
    assert_eq!(head.content.to_string_lossy(), "one\ntwo");
    assert!(head.truncated);
    assert_eq!(head.truncated_by, Some("lines"));
    assert_eq!(head.total_lines, 4.0);
    assert_eq!(head.output_lines, 2);
}

#[test]
fn reports_bytes_when_only_a_trailing_newline_exceeds_limits_at_the_line_cap() {
    let head = truncate_head(&"hello\nworld\n".into(), opts(11.0, 2.0));
    assert_eq!(head.content.to_string_lossy(), "hello\nworld");
    assert!(head.truncated);
    assert_eq!(head.truncated_by, Some("bytes"));
    assert_eq!(head.total_lines, 2.0);
    assert_eq!(head.output_lines, 2);
}

#[test]
fn truncates_head_on_utf8_byte_limits_without_partial_lines() {
    let head = truncate_head(&"éé\nabc".into(), opts(4.0, 10.0));
    assert_eq!(head.content.to_string_lossy(), "éé");
    assert!(head.truncated);
    assert_eq!(head.truncated_by, Some("bytes"));
    assert_eq!(head.output_bytes, 4);
    assert!(!head.first_line_exceeds_limit);
}

#[test]
fn reports_head_truncation_when_the_first_line_exceeds_the_byte_limit() {
    let head = truncate_head(&"éé\nabc".into(), opts(3.0, 10.0));
    assert_eq!(head.content.to_string_lossy(), "");
    assert!(head.truncated);
    assert_eq!(head.truncated_by, Some("bytes"));
    assert!(head.first_line_exceeds_limit);
}

#[test]
fn formats_sizes() {
    assert_eq!(format_size(1023.0), "1023B");
    assert_eq!(format_size(1536.0), "1.5KB");
    assert_eq!(format_size(3.0 * 1024.0 * 1024.0), "3.0MB");
}

#[test]
fn brands_numeric_ids_by_record_kind_and_carries_task_result_types() {
    // Runtime half: IDs serialize as bare numbers. Cross-kind assignment is a compile error in Rust.
    let conversation = ConversationId::from_raw(1);
    let task = TaskId::from_raw(4);
    assert_eq!(serde_json::to_string(&task).unwrap(), "4");
    assert_eq!(serde_json::to_string(&conversation).unwrap(), "1");
    assert_eq!(task.get(), 4);
    assert_eq!(serde_json::from_str::<TaskId>("4").unwrap(), task);
    assert!(serde_json::from_str::<TaskId>("4.5").is_err());
}

#[test]
fn harness_options_telemetry_accepts_one_adapter_and_defaults_to_noop() {
    struct S(JsonString);
    impl RegisteredSection for S {
        fn key(&self) -> &JsonString {
            &self.0
        }
    }
    impl pi_durable::harness::prompt::PromptSection<GenerationPromptInput<NativeTool, S>> for S {
        fn render(
            &self,
            _: Arc<GenerationPromptInput<NativeTool, S>>,
            _: &Context,
        ) -> storage::StorageFuture<Option<JsonString>> {
            Box::pin(async { Ok(None) })
        }
    }
    let registry: Registry<NativeTool, S> = create_registry().unwrap();
    let mut full = HarnessOptions::new(registry, Models::default());
    let adapter = InMemoryTelemetryContext::new();
    let as_context: pi_telemetry::ContextRef = adapter.clone();
    full.telemetry = as_context.clone();
    assert!(Arc::ptr_eq(&full.telemetry, &as_context));
    // Omitted: the no-op context, which still offers propagation and metrics capabilities optionally.
    let registry: Registry<NativeTool, S> = create_registry().unwrap();
    let omitted = HarnessOptions::new(registry, Models::default());
    assert!(!Arc::ptr_eq(&omitted.telemetry, &as_context));
    let _ = omitted.telemetry.metrics();
}

struct Section(JsonString);
impl RegisteredSection for Section {
    fn key(&self) -> &JsonString {
        &self.0
    }
}
impl pi_durable::harness::prompt::PromptSection<GenerationPromptInput<NativeTool, Section>>
    for Section
{
    fn tagged(&self) -> bool {
        false
    }
    fn render(
        &self,
        _: Arc<GenerationPromptInput<NativeTool, Section>>,
        _: &Context,
    ) -> storage::StorageFuture<Option<JsonString>> {
        Box::pin(async { Ok(Some("Be precise.".into())) })
    }
}

/// Offline half of provider-session-cache-e2e (forwarded session ID is the conversation's ProviderDoc ID on every
/// turn) and of system-order-cache-e2e (the system message leads the request). The live cache-hit rates need
/// provider credentials and are not reproducible offline.
#[tokio::test]
async fn forwards_the_provider_session_id_every_turn_and_leads_requests_with_the_system_message() {
    let seen: Arc<Mutex<Vec<(Option<String>, Vec<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let models = Models::default();
    let faux = faux_provider(RegisterFauxProviderOptions {
        tokens_per_second: Some(f64::INFINITY),
        ..Default::default()
    });
    let steps = (0..3)
        .map(|turn| {
            let seen = seen.clone();
            FauxResponseStep::factory(move |context, options, _, _| {
                let roles = context
                    .messages
                    .iter()
                    .map(|m| {
                        serde_json::to_value(m).unwrap()["role"]
                            .as_str()
                            .unwrap()
                            .to_string()
                    })
                    .collect();
                seen.lock()
                    .push((options.session_id.as_ref().map(|s| s.to_string()), roles));
                faux_text_message(format!("answer {turn}"))
            })
        })
        .collect::<Vec<_>>();
    faux.set_responses(steps);
    models.set_provider(faux.provider);
    let registry: Registry<NativeTool, Section> = create_registry().unwrap();
    let mut extension = Extension::new("prompt");
    extension
        .sections
        .push(Arc::new(Section("preamble".into())));
    registry.install(Arc::new(extension)).unwrap();
    let harness = Harness::open(
        Arc::new(MemoryStorage::new()),
        HarnessOptions::new(registry, models),
        &Context::default(),
    )
    .await
    .unwrap();
    let root = harness
        .root(
            ConversationOptions {
                agent: Some(
                    JsonObject::try_from_value(
                        pi_chord::json::parse(
                            r#"{"model":{"provider":"faux","modelId":"faux-1"}}"#,
                        )
                        .unwrap(),
                    )
                    .unwrap(),
                ),
                ..Default::default()
            },
            &Context::default(),
        )
        .await
        .unwrap();
    harness.resume().unwrap();
    for text in ["one", "two", "three"] {
        root.submit(SubmissionDraft::input(text), &Context::default())
            .await
            .unwrap()
            .wait(&Context::default())
            .await
            .unwrap();
    }
    let id = root.id;
    let session_id = root
        .commit(
            move |tx| async move { Ok(tx.doc(&provider_doc(), id).await?.read()?.session_id) },
            &Context::default(),
        )
        .await
        .unwrap()
        .to_string_lossy();
    {
        let seen = seen.lock();
        assert_eq!(seen.len(), 3);
        for (forwarded, roles) in seen.iter() {
            assert_eq!(forwarded.as_deref(), Some(session_id.as_str()));
            assert_eq!(
                roles.first().map(String::as_str),
                Some("system"),
                "{roles:?}"
            );
        }
    }
    harness.close(&Context::default()).await.unwrap();
}
