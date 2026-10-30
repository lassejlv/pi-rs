use pi_durable::{
    env::*,
    tools::{edit::*, edit_diff::*, write::*},
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
#[test]
fn edit_utilities_and_declarations_match_the_independent_pinned_reference() {
    let fixture = pi_chord::json::parse(include_str!("fixtures/edit-reference.json")).unwrap();
    for tool in [create_write_tool(), create_edit_tool()] {
        let expected = fixture["declarations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == encode(&tool.declaration.name))
            .unwrap();
        assert_eq!(encode(&tool.declaration), *expected);
    }
    for (index, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
        let args = &case["args"];
        let run = || -> Result<JsonValue> {
            Ok(match case["operation"].to_utf8().unwrap().as_str() {
                "apply" => encode(&apply_edits_to_normalized_content(
                    &decode(&args["content"]),
                    &decode::<Vec<Edit>>(&args["edits"]),
                    &decode(&args["path"]),
                )?),
                "diff" => encode(&generate_diff_string(
                    &decode(&args["old"]),
                    &decode(&args["new"]),
                    decode(&args["context"]),
                )),
                "patch" => encode(&generate_unified_patch(
                    &decode(&args["path"]),
                    &decode(&args["old"]),
                    &decode(&args["new"]),
                    decode(&args["context"]),
                )),
                "fuzzy" => encode(&fuzzy_find_text(
                    &decode(&args["content"]),
                    &decode(&args["oldText"]),
                )),
                "normalize" => encode(&normalize_for_fuzzy_match(&decode(&args["text"]))),
                other => panic!("unexpected operation {other}"),
            })
        };
        let actual = run().unwrap_or_else(|error| {
            let mut value = pi_chord::json::Map::new();
            value.insert("error".into(), encode(&error.message_text()));
            JsonValue::Object(value.into())
        });
        assert_eq!(actual, case["expected"], "case {index}: {case}");
    }
}
#[tokio::test]
async fn native_write_and_edit_preserve_bom_endings_and_text_encoder_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let env: Arc<dyn ExecutionEnv> = Arc::new(NativeExecutionEnv::new(directory.path()).unwrap());
    let context = Context::default();
    let missing = execute_write(
        WriteToolInput {
            path: "f".into(),
            content: "x".into(),
        },
        None,
        context.clone(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(missing.name(), "Error");
    assert_eq!(
        missing.to_string(),
        "No execution environment is configured"
    );
    let content = JsonString::from_utf16(vec![0xfeff, 97, 13, 10, 0xd800, 98, 13, 10]);
    execute_write(
        WriteToolInput {
            path: "nested/f".into(),
            content,
        },
        Some(env.clone()),
        context.clone(),
    )
    .await
    .unwrap();
    let args =
        pi_chord::json::parse(r#"{"path":"nested/f","edits":[{"oldText":"a","newText":"A"}]}"#)
            .unwrap();
    let result = execute_edit(args, Some(env.clone()), context.clone())
        .await
        .unwrap();
    let actual = env
        .read_binary_file(std::path::Path::new("nested/f"), &context)
        .await
        .unwrap();
    assert_eq!(actual, "\u{feff}A\r\n\u{fffd}b\r\n".as_bytes());
    assert!(result.details.is_some());
    let args =
        pi_chord::json::parse(r#"{"path":"missing","edits":[{"oldText":"a","newText":"A"}]}"#)
            .unwrap();
    let error = execute_edit(args, Some(env), context).await.err().unwrap();
    let ErrorKind::Caused { cause, .. } = error.kind() else {
        panic!("lost edit access cause");
    };
    assert!(matches!(cause.kind(),ErrorKind::File(error) if error.code()==FileErrorCode::NotFound));
}

struct NoSection;
impl pi_durable::harness::registry::RegisteredSection for NoSection {
    fn key(&self) -> &JsonString {
        panic!("no sections are registered")
    }
}
impl
    PromptSection<
        pi_durable::harness::generation::GenerationPromptInput<
            pi_durable::harness::tool::NativeTool,
            NoSection,
        >,
    > for NoSection
{
    fn render(
        &self,
        _: Arc<
            pi_durable::harness::generation::GenerationPromptInput<
                pi_durable::harness::tool::NativeTool,
                NoSection,
            >,
        >,
        _: &Context,
    ) -> storage::StorageFuture<Option<JsonString>> {
        Box::pin(async { Ok(None) })
    }
}
#[tokio::test]
async fn native_harness_runs_the_complete_coding_tools_bundle_and_repairs_model_arguments() {
    use pi_ai::{models::Models, providers::faux::*};
    use pi_durable::harness::tool::NativeTool;
    for retention in [600000.0, 0.0] {
        let directory = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> =
            Arc::new(NativeExecutionEnv::new(directory.path()).unwrap());
        let registry = create_registry::<NativeTool, NoSection>().unwrap();
        let extension = pi_durable::tools::coding_tools::<NoSection>();
        assert_eq!(
            extension
                .tools
                .iter()
                .map(|t| t.declaration.name.to_string_lossy())
                .collect::<Vec<_>>(),
            ["read", "write", "edit", "bash"]
        );
        registry.install(Arc::new(extension)).unwrap();
        let models = Models::default();
        let faux = faux_provider(RegisterFauxProviderOptions {
            tokens_per_second: Some(f64::INFINITY),
            ..Default::default()
        });
        let step = |name: &str, args: serde_json::Value, id: &str| {
            faux_assistant_message(
                vec![faux_tool_call(
                    name,
                    args.as_object().unwrap().clone(),
                    Some(id),
                )],
                FauxMessageOptions {
                    stop_reason: Some(pi_ai::StopReason::ToolUse),
                    ..Default::default()
                },
            )
            .into()
        };
        let checked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = checked.clone();
        faux.set_responses(vec![
            step("write",serde_json::json!({"path":"nested/f","content":"alpha\nbeta\n"}),"write"),
            step("edit",serde_json::json!({"path":"nested/f","edits":"{\"oldText\":\"alpha\",\"newText\":\"ALPHA\"}"}),"edit"),
            step("bash",serde_json::json!({"command":"printf 'shell-output'; printf 'from-shell\\n' >> nested/f"}),"bash"),
            step("read",serde_json::json!({"path":"nested/f"}),"read"),
            FauxResponseStep::Factory(Arc::new(move|context,_,_,_|{
                let transcript=serde_json::to_value(context).unwrap();let results=transcript["messages"].as_array().unwrap().iter().filter(|m|m["role"]=="toolResult").collect::<Vec<_>>();
                assert_eq!(results.len(),4,"{transcript}");assert!(results.iter().all(|r|r["isError"]!=true),"{transcript}");
                let bash=results.iter().find(|r|r["toolName"]=="bash").unwrap();assert_eq!(bash["content"][0]["text"],"shell-output");
                let read=results.iter().find(|r|r["toolName"]=="read").unwrap();assert_eq!(read["content"][0]["text"],"ALPHA\nbeta\nfrom-shell\n");seen.store(true,std::sync::atomic::Ordering::Release);
                Box::pin(async{Ok(faux_text_message("done"))})
            }))
        ]);
        models.set_provider(faux.provider);
        let mut options = HarnessOptions::new(registry, models);
        options.default_document_retention_ms = retention;
        let native = env.clone();
        options.env = Some(Arc::new(move |_, _| {
            let env = native.clone();
            Box::pin(async { Ok(Some(env)) })
        }));
        let context = Context::default();
        let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context)
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
                &context,
            )
            .await
            .unwrap();
        let submission = root
            .submit(
                SubmissionDraft::input("write, edit, run bash and read"),
                &context,
            )
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), submission.wait(&context))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            env.read_text_file(std::path::Path::new("nested/f"), &context)
                .await
                .unwrap(),
            "ALPHA\nbeta\nfrom-shell\n"
        );
        assert!(checked.load(std::sync::atomic::Ordering::Acquire));
        harness.close(&context).await.unwrap();
    }
}
