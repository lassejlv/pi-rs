//! djs-specific embedding regressions beyond the shared codemode contract.

use pi_codemode::{
    CodemodeErrorKind, CodemodeExecuteOptions, CodemodeSandbox, CodemodeSandboxOptions,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fuel_budget_stops_generated_loops_and_uninstrumented_microtasks() {
    let sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
        interrupt_budget: Some(5),
        ..Default::default()
    })
    .expect("create sandbox");
    for code in [
        "Function('for (;;) {}')()",
        r#"Function('for (;;) text("x")')()"#,
        "function again() { Promise.resolve().then(again); } again(); await new Promise(() => {});",
    ] {
        let result = sandbox
            .execute(
                code,
                CodemodeExecuteOptions {
                    timeout_ms: Some(10_000.0),
                    ..Default::default()
                },
            )
            .await
            .expect("execute script");
        let error = result.error().expect("script must exhaust its budget");
        assert_eq!(error.kind, CodemodeErrorKind::Timeout, "{code}: {error:?}");
        assert_eq!(
            error.message, "Execution exceeded its interrupt budget of 5",
            "{code}"
        );
    }
    sandbox.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn arena_cap_bounds_ordinary_arrays_without_arraybuffers() {
    let sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
        memory_limit_bytes: Some(4 * 1024 * 1024),
        ..Default::default()
    })
    .expect("create sandbox");
    let result = sandbox
        .execute(
            "try { new Array(2_000_000).fill(1); return 'allocated'; } catch (error) { return error.name; }",
            CodemodeExecuteOptions::default(),
        )
        .await
        .expect("execute script");
    assert!(result.ok(), "{result:?}");
    assert_eq!(result.value(), Some(&serde_json::json!("InternalError")));
    sandbox.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn date_now_is_an_integer_millisecond() {
    let sandbox = CodemodeSandbox::new(CodemodeSandboxOptions::default()).expect("create sandbox");
    let result = sandbox
        .execute("return Date.now()", CodemodeExecuteOptions::default())
        .await
        .expect("execute script");
    assert!(result.ok(), "{result:?}");
    let now = result
        .value()
        .and_then(|value| value.as_f64())
        .expect("number");
    assert!(now.is_finite() && now == now.trunc(), "{now}");
    sandbox.close().await;
}
