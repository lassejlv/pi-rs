// Golden data for YAML frontmatter parsing (`utils/frontmatter.ts`, which uses the `yaml` package
// 2.9.0): hand-written invalid/edge-case frontmatter plus a deterministic fuzz corpus of mutated
// skill/prompt frontmatter. Each case records the thrown error message or `JSON.stringify` of the
// parsed frontmatter. Written to `yaml_frontmatter.json`; compared by
// `crates/pi-coding-agent/src/utils/yaml/golden_tests.rs`.
//
// Case shape: `{ yaml, error }` or `{ yaml, json }` (plus `approx: true` when the package stringified
// a collection-valued key, which the port only approximates, and `cyclic: true` for cyclic results).
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const REF = process.env.PI_TS_REF;
if (!REF) throw new Error("PI_TS_REF must point at the TypeScript pi checkout");
const here = path.dirname(fileURLToPath(import.meta.url));
const OUT_DIR = process.env.GOLDEN_OUT ?? path.resolve(here, "../../crates/pi-coding-agent/tests/fixtures/golden");
const { parseFrontmatter } = await import(
	pathToFileURL(path.join(REF, "packages/coding-agent/src/utils/frontmatter.ts")).href
);

let warnings = [];
process.emitWarning = (w) => warnings.push(String(w?.message ?? w));

const handWritten = [
	// unterminated / unbalanced flow collections
	"foo: [bar",
	"foo: {bar: 1",
	"foo: [a, b\nbar: 1",
	"foo: {a: 1, b: 2\nbar: 1",
	"[a, b",
	"{a: 1",
	"foo: ]",
	"foo: }",
	"foo: [a,, b]",
	"foo: [, a]",
	"foo: {a: 1,, b: 2}",
	"foo: [a b: c]",
	"foo: {a:1}",
	"foo: [a: 1, b: 2]",
	"foo: [a\n  b]",
	"foo: [a\nb]",
	"foo: {a: [1, 2}, b: 3}",
	// quotes
	"name: \"unterminated",
	"name: 'unterminated",
	"name: \"a\" b",
	"name: 'a' b",
	"name: \"bad \\q escape\"",
	"name: \"bad \\x4 escape\"",
	"name: \"bad \\U0011FFFF escape\"",
	"name: \"ok \\u00e9 \\x41 \\U0001F600\"",
	"name: 'it''s fine'",
	"name: \"multi\n  line\"",
	"name: \"multi\nline\"",
	"name: 'a\n\n  b'",
	// compact / nested mappings
	"description: Use this: when needed",
	"description: Use this skill: for things: and more",
	"a: b: c",
	"a: - b",
	"a: 1\n b: 2",
	"a: 1\n  b: 2",
	"a:\n  b: 1\n c: 2",
	"a:\n  - 1\n - 2",
	"- a\nb: 1",
	"a: 1\n- b",
	"a: 1\na: 2",
	"a: 1\nb: 2\na: 3",
	"? a\n? a",
	"{a: 1, a: 2}",
	"1: a\n\"1\": b",
	"null: a\n~: b",
	"? [a, b]\n: c",
	"? {a: 1}\n: b",
	"? - a\n  - b\n: c",
	"? |\n  block key\n: v",
	"? a\n: b\n? c",
	": no key",
	"a:\n:\n",
	// indentation and tabs
	"a:\n\tb: 1",
	"a:\t1",
	"\ta: 1",
	"a: 1\n\tb: 2",
	"a:\n  b: 1\n    c: 2",
	"a:\n    b: 1\n  c: 2",
	"list:\n  - a\n   - b",
	"list:\n- a\n- b",
	"list:\n  - a\n  -b",
	// reserved / special characters
	"name: @mention",
	"name: `cmd`",
	"name: %percent",
	"name: |pipe",
	"name: >gt",
	"name: ,comma",
	"name: a #comment",
	"name: a#nocomment",
	"name: #empty",
	"# just a comment\nname: x",
	"name: x\n# trailing comment",
	"name: [a, b] # comment",
	"name: \"q\"# comment",
	"name: [a]# comment",
	// anchors, aliases, tags
	"a: &x 1\nb: *x",
	"a: *x",
	"a: &x [1, 2]\nb: *x\nc: *x",
	"a: & 1",
	"a: * ",
	"a: &x &y 1",
	"a: !!str !!int 1",
	"a: &x\n  - 1\n  - 2",
	"a: !!str\n  - 1",
	"a: &x - 1",
	"a: &x: 1",
	"a: *x:",
	"a: &a [*a]",
	"a: &a {b: *a}",
	"a: !!int 12",
	"a: !!int 1.5",
	"a: !!float 1",
	"a: !!float 1.5",
	"a: !!bool yes",
	"a: !!null ''",
	"a: !!str 123",
	"a: !foo bar",
	"a: !<tag:yaml.org,2002:str> 1",
	"a: !<!> 1",
	"a: !e!foo bar",
	"a: !! bar",
	"a: !!binary aGVsbG8=",
	"a: !!binary |\n  aGVs\n  bG8=",
	"a: !!timestamp 2001-12-14",
	"a: !!timestamp 2001-12-14t21:59:43.10-05:00",
	"a: !!timestamp 2001-12-14 21:59:43.10 +5",
	"a: !!timestamp nope",
	"a: !!set {x, y}",
	"a: !!set {x: 1}",
	"a: !!set\n  ? x\n  ? y",
	"a: !!omap [a: 1, b: 2]",
	"a: !!omap [a: 1, a: 2]",
	"a: !!omap\n  - a: 1\n    b: 2",
	"a: !!pairs [a: 1, a: 2]",
	"a: !!map [1]",
	"a: !!seq {b: 1}",
	"a: !!merge <<",
	"base: &b {x: 1, y: 2}\nderived:\n  !!merge <<: *b\n  y: 3",
	"<<: {a: 1}",
	"a: !!binary [1]",
	"a: !!str\n  b: 1",
	"a: !%41 b",
	"a: !e%41 b",
	// block scalars
	"a: |\n  ok\n  lines",
	"a: >\n  folded\n  lines\n\n  para",
	"a: |-\n  strip\n\n",
	"a: |+\n  keep\n\n",
	"a: |2\n    indented",
	"a: |x\n  bad header",
	"a: |1-+\n  bad",
	"a: |\n  first\n less",
	"a: |\n    more\n  less",
	"a: |\n\n    leading empty\n  less",
	"a: >\n  a\n    more\n  b",
	"a: |\nnot indented",
	"a: | # comment\n  text",
	"a: |#comment\n  text",
	"a: |\n  text\n\tb: 1",
	"|\n  root block",
	">\n root folded\n",
	// documents and directives
	"%YAML 1.2\nname: x",
	"%YAML 1.1\nname: x",
	"%YAML 1.3\nname: x",
	"%YAML\nname: x",
	"%TAG !e! tag:example.com,2000:\nname: !e!x y",
	"%FOO bar\nname: x",
	"name: x\n...\nother: y",
	"name: x\n...\n",
	"...\nname: x",
	"--- name: x",
	"--- [a]\n",
	"--- - a",
	"name: x\n--- y",
	"%YAML 1.2",
	// plain scalars and numbers
	"a: 0x1F\nb: 0o17\nc: 017\nd: +12\ne: -0\nf: 1e3\ng: .5\nh: 1.\ni: .inf\nj: -.Inf\nk: .nan\nl: 1_000\nm: 0b101",
	"a: 12345678901234567890\nb: 1.7976931348623157e309\nc: 0x7FFFFFFFFFFFFFFFFF",
	"a: true\nb: True\nc: TRUE\nd: yes\ne: on\nf: ~\ng: Null\nh: NULL\ni: nUll",
	"a: 2001-12-14\nb: 12:30:00",
	"a: \"\"\nb: ''\nc:\nd: ~",
	"a: 'single' \nb: \"double\" ",
	"key with spaces: value with spaces",
	"\"quoted key\": 1\n'single key': 2",
	"a: multi\n  line\n  plain",
	"a: multi\n\n  para",
	"a: b\n  c: d",
	"0: a\n10: b\n2: c\nx: d\n-1: e",
	"__proto__: a\ntoString: b",
	"emoji: 😀 é ü\nkey😀: v",
	"long: " + "x".repeat(120) + ": y",
	"a:\n" + "  b: " + "y".repeat(100) + "\n  c: [" + "z".repeat(90),
	"a: " + "q".repeat(70) + " @bad: x",
	"k".repeat(1100) + ": v",
	"[" + "a, ".repeat(30) + "\n",
	// misc structural errors
	"a: 1\n...\n...",
	"- a\n  - b",
	"- a\n -b",
	"- - a\n  - b",
	"a:\n  - b\n  c: d",
	"a: [b\n, c]",
	"a: {b\n: c}",
	"a: [b: c\n: d]",
	"a: [? b : c]",
	"a: [b, c]: d",
	"[a, b]: c",
	"{a: 1}: b",
	"a: \"b\": c",
	"\"a\": \"b\": c",
	"a: |\n b\n  c: d",
	"? a: b\n: c",
	"a: &x\nb: 1",
	"a: !!str\nb: 1",
	"a: !x\n- b",
	"\u{feff}name: x",
	"name: x\u{feff}",
	"name:\u{a0}x",
	"name: x\r\ndescription: y",
	"name: x\rdescription: y",
];

// Deterministic fuzz corpus: realistic frontmatter mutated with YAML-significant fragments.
const seeds = [
	"name: my-skill\ndescription: Does a thing when asked",
	"name: code-review\ndescription: |\n  Review code for bugs.\n  Use when asked.\nallowed-tools: [read, grep, find]",
	"name: deploy\ndescription: >\n  Deploys the app\n  to production\ndisable-model-invocation: true",
	"description: Summarize the given file\nargument-hint: <file>\nmodel: claude-sonnet",
	"name: agent\ntools:\n  - read\n  - bash\nmodel: gpt-4o\nthinking: high",
	"name: x\nmetadata:\n  version: 1.2\n  tags: [a, b]\n  owner: {team: core, id: 7}",
	"title: \"Quoted: value\"\nsummary: 'single # not comment'\ncount: 42",
	"steps:\n  - name: one\n    run: echo 1\n  - name: two\n    run: |\n      echo 2\n      echo 3",
	"a: &base\n  x: 1\nb: *base\nc: [*base, 2]",
	"? complex key\n: value\nplain: text # trailing comment",
];
const fragments = [
	":", ": ", "- ", "? ", "[", "]", "{", "}", ",", ", ", "#", " #", "&a ", "*a", "!", "!!str ", "!!int ",
	"!!set ", "!!omap ", "!!binary ", "!!timestamp ", "!foo ", "|", ">", "|-", ">+", "'", "\"", "%", "@", "`", "\t",
	"\n", " ", "  ", "    ", "\\", "\\x", "\\u12", "...", "é", "😀", "\u00a0", "&", "*", "0x", "1e3", ".inf", "~",
	"null", "true", "<<: ", "\n  ", "\n- ", "\n? ", "\n: ", "''", "\"\"", "{}", "[]", "a: b", "\n\t", ": |\n  ",
];
let seed = Number(process.env.YAML_FUZZ_SEED ?? 0x9e3779b9);
const rand = () => {
	seed |= 0;
	seed = (seed + 0x6d2b79f5) | 0;
	let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
	t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
	return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const pick = (arr) => arr[Math.floor(rand() * arr.length)];
const mutate = (s) => {
	const kind = rand();
	const pos = Math.floor(rand() * (s.length + 1));
	if (kind < 0.6) return s.slice(0, pos) + pick(fragments) + s.slice(pos);
	if (kind < 0.8) {
		const len = 1 + Math.floor(rand() * 6);
		return s.slice(0, pos) + s.slice(pos + len);
	}
	if (kind < 0.9) {
		const lines = s.split("\n");
		const i = Math.floor(rand() * lines.length);
		lines.splice(i, 0, lines[Math.floor(rand() * lines.length)]);
		return lines.join("\n");
	}
	const lines = s.split("\n");
	const i = Math.floor(rand() * lines.length);
	lines[i] = rand() < 0.5 ? ` ${lines[i]}` : lines[i].replace(/^ /, "");
	return lines.join("\n");
};
const fuzz = [];
const seen = new Set(handWritten);
const FUZZ_CASES = Number(process.env.YAML_FUZZ_CASES ?? 2500);
while (fuzz.length < FUZZ_CASES) {
	let s = pick(seeds);
	const n = 1 + Math.floor(rand() * Number(process.env.YAML_FUZZ_MUTATIONS ?? 3));
	for (let i = 0; i < n; i++) s = mutate(s);
	if (s.includes("\n---") || !s.trim() || !s.isWellFormed() || seen.has(s)) continue;
	seen.add(s);
	fuzz.push(s);
}

const cases = [];
for (const yaml of [...handWritten, ...fuzz]) {
	warnings = [];
	const content = `---\n${yaml}\n---\nBody`;
	const out = { yaml };
	try {
		const { frontmatter } = parseFrontmatter(content);
		try {
			out.json = JSON.stringify(frontmatter) ?? null;
		} catch {
			out.cyclic = true;
		}
	} catch (error) {
		out.error = error instanceof Error ? error.message : String(error);
	}
	if (warnings.some((w) => w.startsWith("Keys with collection values"))) out.approx = true;
	cases.push(out);
}
fs.mkdirSync(OUT_DIR, { recursive: true });
fs.writeFileSync(path.join(OUT_DIR, "yaml_frontmatter.json"), `${JSON.stringify(cases, null, "\t")}\n`);
const errors = cases.filter((c) => c.error).length;
console.log(`yaml_frontmatter.json: ${cases.length} cases (${errors} errors)`);
