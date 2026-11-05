use futures::future::BoxFuture;
use pi_codemode::*;
use pi_json::strict::{JsonString, Value};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct LegacyVm(&'static str);

#[tokio::test]
async fn surrogate_names_remain_distinct_from_real_replacement_characters() {
    let sandbox = CodemodeSandbox::new(Default::default()).unwrap();
    let name = JsonString::from_utf16(vec![0xd800]);
    let replacement = JsonString::from("\u{fffd}");
    let mut options = CodemodeJsonRunOptions::new(123.0, 1, |call, run| {
        run.deliver(
            call.id,
            123.0 + call.id as f64,
            call.id,
            CodemodeJsonOutcome::Ok {
                value: Some(Value::String(call.name)),
            },
        )
        .unwrap();
    });
    options.tools = vec![
        CodemodeJsonDeclaredTool {
            name: name.clone(),
            js_name: Some("first".into()),
            description: None,
        },
        CodemodeJsonDeclaredTool {
            name: replacement.clone(),
            js_name: Some("second".into()),
            description: None,
        },
    ];
    let result = sandbox
        .start_json(
            "return [await tools.first(), await tools.second()];",
            options,
        )
        .unwrap()
        .result()
        .await;
    assert_eq!(result["ok"], Value::Bool(true), "{result}");
    assert_eq!(
        result["value"].as_array().unwrap(),
        &vec![
            Value::String(name.clone()),
            Value::String(replacement.clone())
        ]
    );
    assert_eq!(result["calls"][0]["name"].as_string(), Some(&name));
    assert_eq!(result["calls"][1]["name"].as_string(), Some(&replacement));
    sandbox.close().await;
}
struct Channel;
impl VmChannel for Channel {
    fn post(&self, _: HostToWorkerMessage) {}
    fn stop(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}
impl VmLauncher for LegacyVm {
    fn launch(
        &self,
        data: VmStartData,
        events: VmEvents,
    ) -> BoxFuture<'static, Result<Box<dyn VmChannel>, String>> {
        assert!(data.lossless_json);
        events.message(WorkerToHostMessage::Started {
            fingerprint: self.0.into(),
        });
        events.message(WorkerToHostMessage::Call {
            id: 1,
            target: CodemodeCallTarget::Tool,
            name: "effect".into(),
            args: Some("{}".into()),
        });
        Box::pin(async { Ok(Box::new(Channel) as Box<dyn VmChannel>) })
    }
}
#[tokio::test]
async fn rejects_an_incompatible_vm_before_host_effects() {
    for fingerprint in ["legacy sandbox", "previous sandbox, UTF16 JSON 2"] {
        let sandbox =
            CodemodeSandbox::with_launcher(Default::default(), Arc::new(LegacyVm(fingerprint)))
                .unwrap();
        let effects = Arc::new(AtomicUsize::new(0));
        let seen = effects.clone();
        let mut options = CodemodeJsonRunOptions::new(123.0, 1, move |_, _| {
            seen.fetch_add(1, Ordering::Relaxed);
        });
        options
            .tools
            .push(CodemodeDeclaredTool::new("effect").into());
        let result = sandbox
            .start_json("await tools.effect({});", options)
            .unwrap()
            .result()
            .await;
        assert!(result["error"]["kind"].string_equals("sandbox"), "{result}");
        assert!(
            result["error"]["message"]
                .to_utf8()
                .unwrap()
                .contains("does not support lossless JSON")
        );
        assert_eq!(effects.load(Ordering::Relaxed), 0);
        sandbox.close().await;
    }
}
