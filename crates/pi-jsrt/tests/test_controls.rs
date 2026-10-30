//! Test-runner controls behind `RuntimeOptions` flags: `gc()` and the `__jsrtTimers` fake clock.

use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::json;

fn runtime(expose_gc: bool, timer_control: bool) -> JsRuntime {
    JsRuntime::new(RuntimeOptions {
        expose_gc,
        timer_control,
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
async fn controls_are_absent_by_default() {
    let rt = runtime(false, false);
    assert_eq!(
        rt.eval("[typeof gc, typeof __jsrtTimers]").await.unwrap(),
        json!(["undefined", "undefined"])
    );
}

#[tokio::test]
async fn gc_collects_weak_ref_targets() {
    let rt = runtime(true, false);
    let out = rt
        .eval(
            r#"(async () => {
                let target = { big: new Array(1000).fill(1) };
                const ref = new WeakRef(target);
                target = undefined;
                await new Promise((r) => setTimeout(r, 0));
                gc();
                return ref.deref() === undefined;
            })()"#,
        )
        .await
        .unwrap();
    assert_eq!(out, json!(true));
}

#[tokio::test]
async fn fake_clock_fires_only_when_advanced_and_keeps_real_timers() {
    let rt = runtime(false, true);
    let out = rt
        .eval(
            r#"(async () => {
                const log = [];
                const T = __jsrtTimers;
                setTimeout(() => log.push("real-before"), 5);
                T.freeze(1000);
                setTimeout(() => log.push("fake-50"), 50);
                setTimeout(() => log.push("fake-10"), 10);
                const iv = setInterval(() => log.push("tick@" + T.now()), 30);
                setImmediate(() => log.push("immediate"));
                const counted = T.count();
                // Real timers keep firing while frozen; fake ones wait.
                await new Promise((r) => T.realTimeout(r, 20));
                const waited = log.slice();
                const fired = T.advance(60);
                clearInterval(iv);
                const after = T.count();
                const next = T.next() === undefined;
                // runNext respects the limit and moves the clock to the deadline.
                setTimeout(() => log.push("late@" + T.now()), 100);
                const early = T.runNext(T.now() + 50);
                const late = T.runNext();
                const now = T.now();
                T.thaw();
                setTimeout(() => {}, 0);
                return { counted, waited, fired, after, next, early, late, now, log, frozen: T.frozen() };
            })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        json!({
            "counted": 4,
            "waited": ["real-before"],
            "fired": 5,
            "after": 0,
            "next": true,
            "early": false,
            "late": true,
            "now": 1160,
            "log": ["real-before", "immediate", "fake-10", "tick@1030", "fake-50", "tick@1060", "late@1160"],
            "frozen": false,
        })
    );
}

#[tokio::test]
async fn thaw_drops_pending_fake_timers() {
    let rt = runtime(false, true);
    let out = rt
        .eval(
            r#"(async () => {
                const log = [];
                __jsrtTimers.freeze(0);
                setTimeout(() => log.push("fake"), 0);
                __jsrtTimers.thaw();
                await new Promise((r) => setTimeout(r, 10));
                return log;
            })()"#,
        )
        .await
        .unwrap();
    assert_eq!(out, json!([]));
}
