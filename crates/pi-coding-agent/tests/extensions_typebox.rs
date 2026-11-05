//! Extensions get the real TypeBox 1.3.27: every builder in `fixtures/typebox/cases.mjs` produces
//! the same schema (enumerable JSON in key order plus the hidden `~kind` / `~optional` markers) in
//! pi-jsrt as `typebox` does under Node (`fixtures/typebox/expected.json`, written by
//! `fixtures/typebox/generate.mjs`), and `typebox/value` / `typebox/compile` validate the same.
//! Tool declarations sent to providers are therefore byte-identical to TypeScript pi.

mod extensions_support;

use extensions_support::*;
use serde_json::Value;

const CASES: &str = include_str!("fixtures/typebox/cases.mjs");
const EXPECTED: &str = include_str!("fixtures/typebox/expected.json");

const EXTENSION: &str = r#"
import { Type } from "typebox";
import { Value } from "typebox/value";
import { Compile } from "typebox/compile";
import { Type as SinclairType } from "@sinclair/typebox";
import { Value as SinclairValue } from "@sinclair/typebox/value";
import { Type as AiType, StringEnum } from "@earendil-works/pi-ai";
import { run } from "./cases.mjs";

export default function (pi) {
	const result = run(Type, Value, Compile);
	result.aliases = SinclairType === Type && AiType === Type && SinclairValue === Value;
	pi.registerTool({
		name: "typebox",
		label: "TypeBox",
		description: JSON.stringify(result),
		parameters: Type.Object({
			path: Type.String({ description: "Path to the file" }),
			mode: Type.Optional(StringEnum(["read", "write"], { description: "Mode" })),
			limit: Type.Optional(Type.Integer({ minimum: 1 })),
		}),
		execute: async () => ({ content: [] }),
	});
	pi.registerTool({
		name: "plain",
		label: "Plain",
		description: "plain JSON schema",
		parameters: { type: "object", properties: { limit: { type: "integer" } } },
		execute: async () => ({ content: [] }),
	});
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn typebox_builders_match_real_typebox() {
    let dir = ExtDir::new();
    dir.write("typebox-test/cases.mjs", CASES);
    dir.write("typebox-test/index.ts", EXTENSION);
    let result = dir.discover().await;
    assert!(result.errors.is_empty(), "{:#?}", result.errors);
    let extension = result.extensions[0].read();
    let tool = &extension.tools["typebox"].definition;

    let ours: Value = serde_json::from_str(&tool.description).unwrap();
    let expected: Value = serde_json::from_str(EXPECTED).unwrap();
    let mut mismatches = Vec::new();
    for (name, want) in expected.as_object().unwrap() {
        let got = &ours[name];
        let same = match want.get("error") {
            // Builders TypeBox 1.3.27 does not have must not exist either.
            Some(_) => got.get("error").is_some(),
            None => got == want,
        };
        if !same {
            mismatches.push(format!("{name}:\n  real: {want}\n  ours: {got}"));
        }
    }
    assert!(mismatches.is_empty(), "{} mismatches:\n{}", mismatches.len(), mismatches.join("\n"));
    assert_eq!(ours["aliases"], Value::Bool(true));

    // TypeBox schemas (hidden `~kind`) make validation run `Value.Convert`; plain JSON ones do not.
    assert!(tool.parameters_typebox);
    assert!(!extension.tools["plain"].definition.parameters_typebox);

    // The declaration a provider receives keeps TypeBox's key order.
    assert_eq!(
        serde_json::to_string(&tool.parameters).unwrap(),
        r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string","description":"Path to the file"},"mode":{"type":"string","enum":["read","write"],"description":"Mode"},"limit":{"type":"integer","minimum":1}}}"#
    );
}
