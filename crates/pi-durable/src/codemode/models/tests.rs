use super::*;

struct Catalog;
impl CodemodeModels for Catalog {
    fn models(&self, _: ModelType, _: Option<JsonString>) -> Result<Vec<CatalogModel>> {
        Ok(vec![])
    }
    fn available(
        &self,
        _: ModelType,
        _: Option<JsonString>,
        _: Context,
    ) -> StorageFuture<Vec<CatalogModel>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn model(&self, kind: ModelType, reference: &ModelRef) -> Result<Option<CatalogModel>> {
        Ok((kind==ModelType::Classifier && reference.provider.units()==JsonString::from("p").units() && reference.model_id.units()==JsonString::from("judge").units()).then(|| CatalogModel::new(reference.clone(),"configured authority",||Ok(wire::encode(&serde_json::json!({"id":"judge","provider":"p","headers":{"authorization":"secret"},"baseUrl":"configured"}))))))
    }
    fn request(
        &self,
        _: ModelEffect,
        _: CatalogModel,
        _: JsonValue,
        _: Context,
    ) -> StorageFuture<JsonValue> {
        Box::pin(async { Err(Error::message("No request should run in this test")) })
    }
}
#[test]
fn model_references_use_configured_authority_and_catalogs_drop_headers() {
    let value = wire::encode(
        &serde_json::json!({"provider":"p","id":"judge","baseUrl":"untrusted","headers":{"authorization":"untrusted"}}),
    );
    let model = resolve_model(ModelEffect::Classify, Some(&value), &Catalog).unwrap();
    assert_eq!(*model.value::<&str>().unwrap(), "configured authority");
    let info = model.info().unwrap();
    assert!(wire::get(&info, "headers").is_none());
    assert!(info["baseUrl"].string_equals("configured"));
    for tool in create_model_tools() {
        assert_eq!(tool.callers, Some(vec![ToolCaller::Tools]));
        assert_eq!(tool.replay, ReplayPolicy::Unsafe);
        assert!(
            tool.structured_output_schema
                .unwrap()
                .as_object()
                .unwrap()
                .is_empty()
        );
    }
}
#[tokio::test]
async fn model_errors_preserve_question_ids_model_names_and_object_keys_as_utf16() {
    let invalid = pi_chord::json::parse(
        r#"{"state":{},"questions":{"\ud800":{"instructions":"?","type":"bool","criteria":{}}}}"#,
    )
    .unwrap();
    let error = check_context(ModelEffect::Classify, Some(&invalid)).unwrap_err();
    let expected=pi_chord::json::parse(r#""models.classify() context.questions.\ud800 is a \"bool\" question, so criteria must be { true: string, false: string }. Expected context: { state: { ... }, images?: [{ type: \"image\", data: <base64>, mimeType }], questions: { <id>: { type: \"choice\", instructions, criteria: { <label>: <meaning> } } | { type: \"score\", instructions, criteria: [<lowest level>, ..., <highest level>] } | { type: \"bool\", instructions, criteria: { true: <meaning>, false: <meaning> } } } }.""#).unwrap();
    assert_eq!(error.message_text(), expected.as_string().unwrap().clone());
    let reference = pi_chord::json::parse(r#"{"provider":"p","id":"\udc00"}"#).unwrap();
    let error = resolve_model(ModelEffect::Classify, Some(&reference), &Catalog)
        .err()
        .unwrap();
    assert!(error.message_text().units().contains(&0xdc00));
    let args = pi_chord::json::parse(r#"["classifier",{"\ud800":1}]"#).unwrap();
    let error = catalog_call(
        "models.getModelOfType",
        Some(args),
        Arc::new(Catalog),
        Context::default(),
    )
    .await
    .unwrap_err();
    assert!(error.message_text().units().contains(&0xd800));
}
