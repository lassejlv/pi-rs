//! `structuredClone` keeps an own `"__proto__"` key as a data property (HTML structured clone),
//! instead of setting the clone's prototype. fake-indexeddb clones every stored row with it.

use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn own_proto_key_stays_a_data_property() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let v = rt
        .eval(
            r#"
            const source = JSON.parse('{"__proto__":{"polluted":false},"constructor":{"label":"x"}}');
            const clone = structuredClone(source);
            ({
                own: Object.hasOwn(clone, "__proto__"),
                value: clone.__proto__.polluted,
                prototype: Object.getPrototypeOf(clone) === Object.prototype,
                keys: Object.keys(clone),
            })
            "#,
        )
        .await
        .unwrap();
    assert_eq!(
        v,
        json!({ "own": true, "value": false, "prototype": true, "keys": ["__proto__", "constructor"] })
    );
}
