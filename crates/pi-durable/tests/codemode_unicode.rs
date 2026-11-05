use pi_durable::{
    codemode::{vm::*, *},
    *,
};
use std::sync::Arc;

struct JsonRemote;
impl pi_codemode::CodemodeRemote for JsonRemote {
    fn execute(
        &self,
        request: pi_codemode::CodemodeRemoteRequest,
        exchange: pi_codemode::CodemodeRemoteExchange,
    ) -> futures::future::BoxFuture<'static, std::result::Result<(), String>> {
        let request = serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
        let wrapped: pi_codemode::CodemodeRemoteExchange = Arc::new(move |messages| {
            let messages = serde_json::from_slice(&serde_json::to_vec(&messages).unwrap()).unwrap();
            let reply = exchange(messages);
            Box::pin(async move {
                reply.await.map(|reply| {
                    serde_json::from_slice(&serde_json::to_vec(&reply).unwrap()).unwrap()
                })
            })
        });
        Box::pin(async move {
            pi_codemode::serve_codemode_remote(request, wrapped, Default::default()).await;
            Ok(())
        })
    }
}
fn sandbox(remote: bool) -> NativeSandbox {
    if remote {
        NativeSandbox(
            pi_codemode::CodemodeSandbox::remote(pi_codemode::RemoteCodemodeSandboxOptions::new(
                || Ok(Arc::new(JsonRemote)),
            ))
            .unwrap(),
        )
    } else {
        NativeSandbox::new().unwrap()
    }
}
fn options() -> RunOptions {
    RunOptions {
        start: Start {
            version: CODEMODE_MODULE_VERSION,
            started_at: 123.0,
            seed: 1,
            fingerprint: None,
            deadline: None,
            tools: vec![],
            globals: vec![],
            namespaces: vec![],
            max_output_tokens: 1000.0,
            store: JsonObject::default(),
        },
        timeout_ms: Some(5000.0),
        signal: pi_chord::AbortController::new().signal(),
        on_started: Arc::new(|_| {}),
        on_call: Arc::new(|_| panic!("No host call expected")),
        on_idle: Arc::new(|_, _| {}),
    }
}

#[tokio::test]
async fn invalid_global_names_preserve_the_source_error_units_before_execution() {
    for remote in [false, true] {
        let sandbox = sandbox(remote);
        let mut config = options();
        config.start.globals.push(DeclaredGlobal {
            name: JsonString::from_utf16(vec![0xd800]),
            spread: false,
        });
        let error = sandbox.start("return 1;".into(), config).err().unwrap();
        let expected = JsonString::from_utf16(
            "Invalid global name \""
                .encode_utf16()
                .chain([0xd800, 34])
                .collect(),
        );
        assert_eq!(error.name(), "Error");
        assert_eq!(error.message_text(), expected);
        sandbox.0.close().await;
    }
}

#[tokio::test]
async fn native_tool_metadata_and_call_records_preserve_utf16_across_both_transports() {
    for remote in [false, true] {
        let sandbox = sandbox(remote);
        let name = JsonString::from_utf16(vec![0xd800]);
        let alias = JsonString::from_utf16(vec![0xd802]);
        let description = JsonString::from_utf16(vec![0xdc01]);
        let reply = JsonString::from_utf16(vec![0xd804]);
        let mut config = options();
        config.start.tools.push(DeclaredTool {
            name: name.clone(),
            js_name: alias.clone(),
            description: description.clone(),
            structured: false,
            hidden: false,
        });
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        config.on_call = Arc::new(move |call| {
            send.send(call).unwrap();
        });
        let run = sandbox.start(
            "const tool = ALL_TOOLS[0]; const value = await tools[tool.name]({input:'\\udc03'}); return [tool.name, tool.description, value];".into(),
            config,
        ).unwrap();
        let call = tokio::time::timeout(std::time::Duration::from_secs(3), receive.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(call.name, name);
        assert_eq!(
            call.args.unwrap()["input"].as_string().unwrap().units(),
            &[0xdc03]
        );
        run.deliver(
            1,
            124.0,
            call.id,
            Outcome::Success(Some(JsonValue::String(reply.clone()))),
        )
        .unwrap();
        let result = run.result().await.unwrap();
        assert_eq!(result["ok"], JsonValue::Bool(true), "{result}");
        let values = result["value"].as_array().unwrap();
        assert_eq!(
            values,
            &vec![
                JsonValue::String(alias),
                JsonValue::String(description),
                JsonValue::String(reply)
            ]
        );
        assert_eq!(result["calls"][0]["name"].as_string(), Some(&name));
        sandbox.0.close().await;
    }
}

#[tokio::test]
async fn source_validation_preserves_units_but_respects_provider_errors_and_changed_schemas() {
    use pi_durable::harness::tool::{ExecutableTool, ToolCall};
    let sandbox = Arc::new(NativeSandbox::new().unwrap());
    let mut config = CodemodeToolOptions::new(sandbox.clone());
    config.models = false;
    let mut tool = create_codemode_tool(config);
    let mut arguments = JsonObject::default();
    let source = JsonString::from_utf16(
        "return '"
            .encode_utf16()
            .chain([0xd800])
            .chain("';".encode_utf16())
            .collect(),
    );
    arguments.insert("code".into(), JsonValue::String(source.clone()));
    let mut call = ToolCall::new("source", "codemode", arguments.value().clone());
    let validated = tool
        .validate_arguments(&call, arguments.value().clone())
        .unwrap();
    assert_eq!(
        validated.value()["code"].as_string().unwrap().units(),
        source.units()
    );
    call.extra.insert(
        "argumentsError".into(),
        "provider rejected the arguments".into(),
    );
    assert!(
        tool.validate_arguments(&call, arguments.value().clone())
            .unwrap_err()
            .to_string()
            .contains("provider rejected the arguments")
    );
    call.extra = JsonObject::default();
    tool.declaration
        .parameters
        .as_object_mut()
        .unwrap()
        .insert("maxProperties".into(), 0.into());
    assert!(
        tool.validate_arguments(&call, arguments.value().clone())
            .is_err()
    );
    sandbox.0.close().await;
}

#[tokio::test]
async fn native_sandbox_accepts_raw_unpaired_source_in_literals_regexes_and_templates() {
    for remote in [false, true] {
        let sandbox = sandbox(remote);
        let mut source: Vec<u16> = "const value = '".encode_utf16().chain([0xd800]).collect();
        source.extend("'; const pattern = /".encode_utf16());
        source.push(0xdc01);
        source.extend("/; const raw = String.raw`".encode_utf16());
        source.push(0xd802);
        source.extend("`; return [value, pattern.source, raw];".encode_utf16());
        let run = sandbox
            .start(JsonString::from_utf16(source), options())
            .unwrap();
        let result = run.result().await.unwrap();
        assert_eq!(result["ok"], JsonValue::Bool(true), "{result}");
        assert_eq!(result["value"].as_array().unwrap().len(), 3);
        for (value, unit) in result["value"]
            .as_array()
            .unwrap()
            .iter()
            .zip([0xd800, 0xdc01, 0xd802])
        {
            assert_eq!(value.as_string().unwrap().units(), &[unit]);
        }
        sandbox.0.close().await;
    }
}

#[tokio::test]
async fn native_utf16_source_loops_still_observe_deadlines() {
    for remote in [false, true] {
        let sandbox = sandbox(remote);
        let mut config = options();
        config.timeout_ms = Some(100.0);
        let source: Vec<u16> = "//"
            .encode_utf16()
            .chain([0xd800])
            .chain("\nwhile (true) await null;".encode_utf16())
            .collect();
        let run = sandbox
            .start(JsonString::from_utf16(source), config)
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), run.result())
            .await
            .unwrap()
            .unwrap();
        assert!(result["error"]["kind"].string_equals("timeout"), "{result}");
        sandbox.0.close().await;
    }
}

#[tokio::test]
async fn native_sandbox_preserves_unpaired_units_in_output_values_and_store_writes() {
    for remote in [false, true] {
        let sandbox = sandbox(remote);
        let run = sandbox
        .start(
            r#"text("\ud800"); store("\ud801", {value: "\udc00"}); return {"\ud802": "\udc01"};"#
                .into(),
            options(),
        )
        .unwrap();
        let result = run.result().await.unwrap();
        sandbox.0.close().await;
        assert_eq!(result["ok"], JsonValue::Bool(true), "{result}");
        assert_eq!(
            result["output"][0]["text"].as_string().unwrap().units(),
            &[0xd800]
        );
        let key = JsonString::from_utf16(vec![0xd802]);
        assert_eq!(
            result["value"].as_object().unwrap()[&key]
                .as_string()
                .unwrap()
                .units(),
            &[0xdc01]
        );
        let key = JsonString::from_utf16(vec![0xd801]);
        assert_eq!(
            result["storeWrites"]["set"].as_object().unwrap()[&key]["value"]
                .as_string()
                .unwrap()
                .units(),
            &[0xdc00]
        );
    }
}

#[tokio::test]
async fn native_sandbox_preserves_call_arguments_results_errors_properties_and_loaded_keys() {
    for remote in [false, true] {
        let sandbox = sandbox(remote);
        let mut config = options();
        config.start.tools.push(DeclaredTool {
            name: "echo".into(),
            js_name: "echo".into(),
            description: "Echo".into(),
            structured: true,
            hidden: false,
        });
        let key = JsonString::from_utf16(vec![0xd801]);
        config
            .start
            .store
            .insert(key, JsonValue::String(JsonString::from_utf16(vec![0xdc02])));
        let (send, mut calls) = tokio::sync::mpsc::unbounded_channel();
        config.on_call = Arc::new(move |call| {
            send.send(call).unwrap();
        });
        let run=sandbox.start(r#"const echoed=await tools.echo({"\ud800":"\udc00"}); try { await tools.echo(null); } catch(error) { return {echoed,message:error.message,detail:error.detail,loaded:load("\ud801")}; }"#.into(),config).unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), calls.recv())
            .await
            .unwrap()
            .unwrap();
        let key = JsonString::from_utf16(vec![0xd800]);
        assert_eq!(
            first.args.as_ref().unwrap().as_object().unwrap()[&key]
                .as_string()
                .unwrap()
                .units(),
            &[0xdc00]
        );
        run.deliver(1, 124.0, first.id, Outcome::Success(first.args))
            .unwrap();
        let second = tokio::time::timeout(std::time::Duration::from_secs(5), calls.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.args, Some(JsonValue::Null));
        let mut properties = JsonObject::default();
        properties.insert(
            "detail".into(),
            JsonValue::String(JsonString::from_utf16(vec![0xdc03])),
        );
        run.deliver(
            2,
            125.0,
            second.id,
            Outcome::Failure {
                message: JsonString::from_utf16(vec![0xd803]),
                properties: Some(properties),
            },
        )
        .unwrap();
        let result = run.result().await.unwrap();
        sandbox.0.close().await;
        assert_eq!(result["ok"], JsonValue::Bool(true), "{result}");
        assert_eq!(
            result["value"]["echoed"].as_object().unwrap()[&key]
                .as_string()
                .unwrap()
                .units(),
            &[0xdc00]
        );
        for (field, unit) in [("message", 0xd803), ("detail", 0xdc03), ("loaded", 0xdc02)] {
            assert_eq!(result["value"][field].as_string().unwrap().units(), &[unit]);
        }
    }
}

#[tokio::test]
async fn native_sandbox_preserves_script_error_fields_and_distinguishes_null_from_undefined() {
    for remote in [false, true] {
        let sandbox = sandbox(remote);
        let run = sandbox
            .start(
                r#"throw Object.assign(new Error("\ud800"), {name:"\udc00"});"#.into(),
                options(),
            )
            .unwrap();
        let result = run.result().await.unwrap();
        assert!(result["error"]["kind"].string_equals("script"), "{result}");
        assert_eq!(
            result["error"]["message"].as_string().unwrap().units(),
            &[0xd800]
        );
        assert_eq!(
            result["error"]["name"].as_string().unwrap().units(),
            &[0xdc00]
        );
        let parsed=sandbox.start(r#"return [JSON.parse('"' + String.fromCharCode(0xd800) + '"').charCodeAt(0),JSON.parse("1e400") === Infinity];"#.into(),options()).unwrap().result().await.unwrap();
        assert_eq!(parsed["value"][0], JsonValue::from(0xd800usize));
        assert_eq!(parsed["value"][1], JsonValue::Bool(true));
        for (code, present) in [("return null;", true), ("return undefined;", false)] {
            let result = sandbox
                .start(code.into(), options())
                .unwrap()
                .result()
                .await
                .unwrap();
            assert_eq!(
                result
                    .as_object()
                    .unwrap()
                    .contains_key(&JsonString::from("value")),
                present
            );
        }
        sandbox.0.close().await;
    }
}
