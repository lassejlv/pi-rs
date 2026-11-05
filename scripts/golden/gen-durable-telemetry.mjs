// Expectations produced by the pinned telemetry adapter and Durable vocabulary.
import assert from "node:assert/strict";
import {execFileSync} from "node:child_process";
import {readFile,writeFile} from "node:fs/promises";
import {join,resolve} from "node:path";
import {fileURLToPath,pathToFileURL} from "node:url";
import {createHash} from "node:crypto";
const root=fileURLToPath(new URL("../../",import.meta.url)),reference=resolve(process.argv[2]??"../pi-mono"),pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);
const sources=["packages/durable/src/harness/telemetry.ts","packages/telemetry/src/index.ts","packages/telemetry/src/memory.ts"],hashes={};
for(const path of sources){const bytes=await readFile(join(reference,path));assert.deepEqual(bytes,execFileSync("git",["show",`${pin}:${path}`],{cwd:reference}));hashes[path]=createHash("sha256").update(bytes).digest("hex");}
const {DURABLE_TELEMETRY_SCHEMA:schema}=await import(pathToFileURL(join(reference,sources[0])));
const {InMemoryTelemetryContext}=await import(pathToFileURL(join(reference,sources[1])));
const high=String.fromCharCode(0xd800),low=String.fromCharCode(0xdc01),attributes={text:high,array:[low,"\u{1f600}"]},diagnosticInput="a".repeat(499)+"\u{1f600} tail";
const telemetry=new InMemoryTelemetryContext();
await telemetry.startSpan({name:"test.unicode",attributes},span=>{span.addEvent("test.event",attributes);span.setStatus({status:"error",error:{name:"task_faulted",message:diagnosticInput.slice(0,500)}});});
const snapshot=telemetry.getSpans()[0],unicode={attributesJson:JSON.stringify(snapshot.attributes),eventAttributesJson:JSON.stringify(snapshot.events[0].attributes),statusJson:JSON.stringify(snapshot.status),diagnosticInput};
const writes=[["crates/pi-durable/src/harness/telemetry_schema.json",schema],["crates/pi-durable/tests/fixtures/telemetry-schema-reference.json",{reference:pin,hashes,schema}],["crates/pi-telemetry/tests/fixtures/utf16-reference.json",{reference:pin,hashes,unicode}]];
for(const [path,value] of writes){const text=JSON.stringify(value,null,2)+"\n";if(process.argv.includes("--check"))assert.equal(await readFile(join(root,path),"utf8"),text,`Stale telemetry fixture ${path}`);else await writeFile(join(root,path),text);}
for(const path of sources)assert.equal(createHash("sha256").update(await readFile(join(reference,path))).digest("hex"),hashes[path]);
console.log("Pinned Durable vocabulary and UTF-16 telemetry expectations verified");
