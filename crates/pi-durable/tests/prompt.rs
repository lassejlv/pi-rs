use parking_lot::Mutex;
use pi_chord::{AbortController, context::with_abort_signal};
use pi_durable::{
    harness::{
        agent::AgentSection,
        context::ContextView,
        declaration::{ToolDeclaration, current_tool_declarations},
        prompt::{PromptSection, Sections, plan_system_entries, render_sections, replay_sections},
        registry::RegisteredSection,
    },
    storage::StorageFuture,
    *,
};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::Arc;

fn decode<T: DeserializeOwned>(value: &JsonValue) -> T {
    serde_json::from_str(&value.to_string()).unwrap()
}
fn encode(value: &impl Serialize) -> JsonValue {
    pi_chord::json::to_value(value).unwrap()
}
fn sections(value: &JsonValue) -> Sections {
    decode::<Vec<(JsonString, JsonString)>>(value)
        .into_iter()
        .collect()
}
fn pairs(value: &Sections) -> JsonValue {
    encode(&value.iter().collect::<Vec<_>>())
}

#[test]
fn native_plans_and_replay_match_independent_pinned_source_cases() {
    let fixture: JsonValue = serde_json::from_str(include_str!("fixtures/prompt.json")).unwrap();
    assert!(fixture["reference"].string_equals("5e63241d0294ed3fcfc4388c737a8ea27e09c74e"));
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 142);
    for case in cases {
        let view = &case["view"];
        let mut view = ContextView {
            head: decode(&view["head"]),
            entries: decode(&view["entries"]),
            messages: decode(&view["messages"]),
            contributions: Vec::new(),
        };
        let shown = replay_sections(&view.messages).unwrap();
        assert_eq!(pairs(&shown), case["shown"], "{}", case["name"]);
        let drafts = plan_system_entries(
            &view,
            &sections(&case["desired"]),
            &decode::<Vec<ToolDeclaration>>(&case["tools"]),
            7.5,
        )
        .unwrap();
        let mut encoded = encode(&drafts);
        for draft in encoded.as_array_mut().unwrap() {
            draft
                .as_object_mut()
                .unwrap()
                .shift_remove(&JsonString::from("kind"));
        }
        assert_eq!(encoded, case["drafts"], "{}", case["name"]);
        // Object key order matters inside declarations, where the reference
        // explicitly compares JSON.stringify output. Envelope field order does
        // not affect replay, including ContextEdit's action/target fields.
        for (actual, expected) in encoded
            .as_array()
            .unwrap()
            .iter()
            .zip(case["drafts"].as_array().unwrap())
        {
            assert_eq!(
                actual["model"].to_string(),
                expected["model"].to_string(),
                "{}",
                case["name"]
            );
        }
        for draft in drafts {
            view.messages
                .extend(draft.model.unwrap_or_default().into_iter().map(Arc::new));
        }
        assert_eq!(
            pairs(&replay_sections(&view.messages).unwrap()),
            case["replayed"],
            "{}",
            case["name"]
        );
        assert_eq!(
            encode(&current_tool_declarations(&view.messages).unwrap()).to_string(),
            case["offered"].to_string(),
            "{}",
            case["name"]
        );
    }
}

type Renderer =
    dyn Fn(Arc<Mutex<Vec<JsonString>>>, Context) -> StorageFuture<Option<JsonString>> + Send + Sync;
struct Section {
    key: JsonString,
    tag: bool,
    render: Arc<Renderer>,
}
impl RegisteredSection for Section {
    fn key(&self) -> &JsonString {
        &self.key
    }
}
impl PromptSection<Mutex<Vec<JsonString>>> for Section {
    fn tagged(&self) -> bool {
        self.tag
    }
    fn render(
        &self,
        input: Arc<Mutex<Vec<JsonString>>>,
        context: &Context,
    ) -> StorageFuture<Option<JsonString>> {
        (self.render)(input, context.clone())
    }
}
fn section(key: impl Into<JsonString>, result: Result<Option<JsonString>>) -> Section {
    Section {
        key: key.into(),
        tag: true,
        render: Arc::new(move |_, _| {
            let result = result.clone();
            Box::pin(async move { result })
        }),
    }
}

#[tokio::test]
async fn renderers_run_sequentially_with_shared_input_and_context() {
    let key = pi_chord::context::create_context_key::<u64>("prompt test");
    let context = pi_chord::context::with_context_value(&key, 42, &Context::default());
    let input = Arc::new(Mutex::new(Vec::new()));
    let renderers = (0..3)
        .map(|i| {
            let key = key.clone();
            let expected = input.clone();
            Section {
                key: format!("s{i}").into(),
                tag: i != 1,
                render: Arc::new(move |input, context| {
                    assert!(Arc::ptr_eq(&input, &expected));
                    assert_eq!(*context.value(&key).unwrap(), 42);
                    Box::pin(async move {
                        assert_eq!(input.lock().len(), i);
                        tokio::task::yield_now().await;
                        input.lock().push(format!("{i}").into());
                        Ok((i != 2).then(|| format!("text{i}").into()))
                    })
                }),
            }
        })
        .collect::<Vec<_>>();
    let desired = render_sections(
        &renderers,
        input.clone(),
        &Sections::new(),
        |_, _| panic!("unexpected report"),
        &context,
    )
    .await
    .unwrap();
    assert_eq!(
        desired.into_iter().collect::<Vec<_>>(),
        vec![
            ("s0".into(), "<s0>\ntext0\n</s0>".into()),
            ("s1".into(), "text1".into())
        ]
    );
    assert_eq!(input.lock().len(), 3);
}

#[tokio::test]
async fn failures_keep_shown_text_and_report_original_error_and_section() {
    let failure = Error::message("render failed");
    let sections = vec![
        section("failed", Err(failure.clone())),
        section("new", Err(Error::message("new failure"))),
        section("good", Ok(Some("text".into()))),
    ];
    let shown = Sections::from_iter([
        ("failed".into(), "already tagged".into()),
        ("stale".into(), "old".into()),
    ]);
    let mut reported = Vec::new();
    let desired = render_sections(
        &sections,
        Arc::new(Mutex::new(Vec::new())),
        &shown,
        |error, owner| {
            reported.push((error, owner as *const _));
            Ok(())
        },
        &Context::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        desired.into_iter().collect::<Vec<_>>(),
        vec![
            ("failed".into(), "already tagged".into()),
            ("good".into(), "<good>\ntext\n</good>".into())
        ]
    );
    assert!(reported[0].0.same_error(&failure));
    assert_eq!(reported[0].1, &sections[0] as *const _);
    assert_eq!(reported.len(), 2);
}

#[tokio::test]
async fn reporter_failure_propagates_without_rendering_later_sections() {
    let failure = Error::message("report failed");
    let mut late = section("late", Ok(None));
    late.render = Arc::new(|_, _| panic!("later renderer must not run"));
    let result = render_sections(
        &[section("failed", Err(Error::message("render"))), late],
        Arc::new(Mutex::new(Vec::new())),
        &Sections::new(),
        |_, _| Err(failure.clone()),
        &Context::default(),
    )
    .await
    .unwrap_err();
    assert!(result.same_error(&failure));
}

#[tokio::test]
async fn cancellation_preserves_render_error_and_does_not_reject_successful_rendering() {
    let controller = AbortController::new();
    controller.abort();
    let context = with_abort_signal(controller.signal(), &Context::default());
    let failure = Error::message("original renderer error");
    let result = render_sections(
        &[section("a", Err(failure.clone()))],
        Arc::new(Mutex::new(Vec::new())),
        &Sections::new(),
        |_, _| panic!("cancelled rendering must not report"),
        &context,
    )
    .await
    .unwrap_err();
    assert!(result.same_error(&failure));
    let result = render_sections(
        &[section("a", Ok(Some("text".into())))],
        Arc::new(Mutex::new(Vec::new())),
        &Sections::new(),
        |_, _| panic!("unexpected report"),
        &context,
    )
    .await
    .unwrap();
    assert_eq!(result.len(), 1);
}

#[tokio::test]
async fn instructions_are_tagged_like_any_section_and_section_tags_preserve_utf16_units() {
    let key = JsonString::from_utf16(vec![0xd800]);
    let text = JsonString::from_utf16(vec![0xdfff]);
    let sections = vec![
        AgentSection::Instructions("You are helpful.".into()),
        AgentSection::Registered(Arc::new(section(key.clone(), Ok(Some(text))))),
    ];
    let desired = render_sections(
        &sections,
        Arc::new(Mutex::new(Vec::new())),
        &Sections::new(),
        |_, _| panic!("unexpected report"),
        &Context::default(),
    )
    .await
    .unwrap();
    assert!(
        desired[&JsonString::from("instructions")]
            .equals_str("<instructions>\nYou are helpful.\n</instructions>")
    );
    assert_eq!(
        desired[&key].units(),
        &[60, 0xd800, 62, 10, 0xdfff, 10, 60, 47, 0xd800, 62]
    );
}
