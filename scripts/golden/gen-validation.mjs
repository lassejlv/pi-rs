// Golden data for tool argument validation with TypeBox-built schemas (`validateToolArguments` of
// `packages/ai/src/utils/validation.ts`, which runs TypeBox 1.3.27 `Value.Convert` before the
// JSON Schema coercion). Written to `crates/pi-ai/tests/fixtures/typebox_validation.json`;
// compared by `crates/pi-ai/tests/typebox_validation.rs`.
//
// Case shape: `{ name, parameters, arguments, ok }` or `{ ..., error }` where `parameters` is the
// schema's JSON (TypeBox's `~kind` markers are non-enumerable and do not survive it).
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const REF = process.env.PI_TS_REF;
if (!REF) throw new Error("PI_TS_REF must point at the TypeScript pi checkout");
const here = path.dirname(fileURLToPath(import.meta.url));
const OUT = path.resolve(here, "../../crates/pi-ai/tests/fixtures/typebox_validation.json");
const { validateToolArguments } = await import(pathToFileURL(path.join(REF, "packages/ai/src/utils/validation.ts")).href);
const { Type } = await import(pathToFileURL(path.join(REF, "node_modules/typebox/build/index.mjs")).href);

const schemas = {
	primitives: Type.Object({
		n: Type.Optional(Type.Number()),
		i: Type.Optional(Type.Integer()),
		b: Type.Optional(Type.Boolean()),
		s: Type.Optional(Type.String()),
		z: Type.Optional(Type.Null()),
	}),
	arrays: Type.Object({
		paths: Type.Array(Type.String()),
		numbers: Type.Optional(Type.Array(Type.Number())),
		pair: Type.Optional(Type.Tuple([Type.String(), Type.Number()])),
	}),
	edit: Type.Object({
		path: Type.String(),
		edits: Type.Array(Type.Object({ oldText: Type.String(), newText: Type.String() })),
	}),
	unions: Type.Object({
		u: Type.Union([Type.Number(), Type.Boolean()]),
		e: Type.Optional(Type.Enum(["a", "b", 1])),
		l: Type.Optional(Type.Literal(3)),
		lb: Type.Optional(Type.Literal(true)),
		ls: Type.Optional(Type.Literal("x")),
	}),
	records: Type.Object({
		env: Type.Record(Type.String(), Type.Number()),
		extra: Type.Optional(Type.Object({ known: Type.Number() }, { additionalProperties: Type.Boolean() })),
	}),
	nullable: Type.Object({
		maybe: Type.Union([Type.String(), Type.Null()]),
		count: Type.Optional(Type.Integer({ minimum: 0 })),
	}),
	intersect: Type.Intersect([Type.Object({ a: Type.Number() }), Type.Object({ b: Type.Boolean() })]),
};

const values = [
	{ n: "12" }, { n: "" }, { n: " 7 " }, { n: "0x1A" }, { n: "1e3" }, { n: "abc" }, { n: "Infinity" }, { n: "12n" },
	{ n: "-3.5n" }, { n: "TRUE" }, { n: true }, { n: null }, { n: [1] }, { n: "9007199254740993n" },
	{ i: "1.9" }, { i: "-1.9" }, { i: 2.7 }, { i: "" }, { i: "3n" }, { i: "1.5.2" },
	{ b: "TRUE" }, { b: "1" }, { b: "0" }, { b: "yes" }, { b: 1 }, { b: 2 }, { b: null }, { b: "false" },
	{ s: 5 }, { s: true }, { s: null }, { s: { x: 1 } }, { s: 1.5e21 },
	{ z: "" }, { z: "null" }, { z: "UNDEFINED" }, { z: 0 }, { z: false }, { z: "0" }, { z: 1 },
	{ paths: "a.txt" }, { paths: ["a", 1, true] }, { paths: 3 }, { paths: null }, { numbers: "4" }, { paths: [], numbers: ["1", "x"] },
	{ paths: [], pair: [5, "6"] }, { paths: [], pair: "x" },
	{ path: "f", edits: { oldText: "a", newText: 1 } }, { path: 1, edits: [{ oldText: 2, newText: null }] },
	{ path: "f", edits: "[]" },
	{ u: "true" }, { u: "3" }, { u: "x" }, { u: "1" }, { u: true, e: "1" }, { u: 1, e: 1 }, { u: 1, e: "c" }, { u: 1, l: "3" },
	{ u: 1, l: "4" }, { u: 1, lb: "true" }, { u: 1, lb: 1 }, { u: 1, ls: "x" }, { u: 1, ls: 5 },
	{ env: { A: "1", B: "x" } }, { env: [] }, { env: { A: 1 }, extra: { known: "2", other: "true" } },
	{ maybe: "" }, { maybe: 5 }, { maybe: null }, { maybe: "null", count: "2" }, { maybe: "s", count: "-1" },
	{ a: "1", b: "true" }, { a: "x", b: "0" },
];

const cases = [];
for (const [name, schema] of Object.entries(schemas)) {
	for (const args of values) {
		const toolCall = { type: "toolCall", id: "call", name, arguments: structuredClone(args) };
		const out = { name, parameters: JSON.parse(JSON.stringify(schema)), arguments: args };
		try {
			out.ok = validateToolArguments({ name, description: name, parameters: schema }, toolCall);
		} catch (error) {
			out.error = error.message;
		}
		cases.push(out);
	}
}
fs.mkdirSync(path.dirname(OUT), { recursive: true });
fs.writeFileSync(OUT, `${JSON.stringify(cases, null, "\t")}\n`);
console.log(`typebox_validation.json: ${cases.length} cases`);
