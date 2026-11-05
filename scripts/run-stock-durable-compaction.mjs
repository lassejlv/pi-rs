// Run the entire pinned compaction file against native tasks and Session services.
// Original models, callbacks and controlled/persistent I/O remain host capabilities.
import assert from "node:assert/strict";
import {execFileSync,spawn} from "node:child_process";
import {mkdtemp,writeFile,readFile,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {resolve,join} from "node:path";
import {fileURLToPath} from "node:url";
import {createHash} from "node:crypto";
const root=fileURLToPath(new URL("../",import.meta.url));
const reference=resolve(process.argv[2]??"../pi-mono");
const pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);
const test=join(reference,"packages/durable/test/harness-compaction.test.ts");const before=await readFile(test);
assert.deepEqual(before,execFileSync("git",["show",`${pin}:packages/durable/test/harness-compaction.test.ts`],{cwd:reference}),"Stock compaction tests differ from pin");
// Reuse the optimized bridge profile of the other stock suites. Upstream
// assertions, flushes, timers and test deadlines remain unchanged.
const profile=process.argv.includes("--debug")?"debug":"release";
execFileSync("cargo",["build",...(profile==="release"?["--release"]:[]),"--locked","-q","-p","pi-durable","--example","task_stock_bridge"],{cwd:root,stdio:"inherit"});
const target=process.env.CARGO_TARGET_DIR??JSON.parse(execFileSync("cargo",["metadata","--no-deps","--format-version","1"],{cwd:root,encoding:"utf8"})).target_directory;
const temporary=await mkdtemp(join(tmpdir(),"pi-durable-stock-compaction-"));
const adapter=fileURLToPath(new URL("stock/durable-compaction-adapter.mjs",import.meta.url));
const tasks=fileURLToPath(new URL("stock/durable-task-adapter.mjs",import.meta.url));
const hook=join(temporary,"setup.mjs");
await writeFile(hook,`import {afterAll} from ${JSON.stringify(join(reference,"node_modules/vitest/dist/index.js"))};import {shutdown} from ${JSON.stringify(tasks)};afterAll(shutdown,60000);`);
const config=join(temporary,"vitest.config.mjs");
const index=join(reference,"packages/durable/src/index.ts"),barrel=join(temporary,"native-index.mjs");
await writeFile(barrel,`export * from ${JSON.stringify(index)};export {Harness,watchEvents} from ${JSON.stringify(tasks)};export {createRegistry} from ${JSON.stringify(fileURLToPath(new URL("stock/durable-registry-adapter.mjs",import.meta.url)))};import {MemoryStorage as SourceMemory} from ${JSON.stringify(join(reference,"packages/durable/src/storage/memory.ts"))};import {createNativeMemoryAdapter} from ${JSON.stringify(fileURLToPath(new URL("stock/durable-memory-adapter.mjs",import.meta.url)))};export const MemoryStorage=createNativeMemoryAdapter(SourceMemory);`);
await writeFile(config,`import original from ${JSON.stringify(join(reference,"packages/durable/vitest.config.ts"))};import {existsSync} from "node:fs";import {resolve,dirname} from "node:path";
export default {...original,resolve:{...original.resolve,alias:original.resolve.alias.map(alias=>alias.find.test("@earendil-works/pi-durable")?{...alias,replacement:${JSON.stringify(barrel)}}:alias)},plugins:[{name:"rust-compaction-under-stock-tests",enforce:"pre",resolveId(source,importer){
const path=source.startsWith(".")&&importer?resolve(dirname(importer),source):source;
if(source==="pi-durable-stock-reference-compaction")return ${JSON.stringify(join(reference,"packages/durable/src/harness/compaction.ts"))}+"?stockReference";
if(path===${JSON.stringify(join(reference,"packages/durable/src/harness/compaction.ts"))})return ${JSON.stringify(adapter)};
const match=/^@earendil-works\\/(pi-ai|pi-telemetry|pi-codemode|chord)(?:\\/(.*))?$/.exec(source);
if(match){const directory=match[1].replace(/^pi-/,"");const file=${JSON.stringify(join(reference,"packages"))}+"/"+directory+"/src/"+(match[2]??"index");return [file+".ts",file+"/index.ts"].find(existsSync);}
}}],test:{...original.test,include:[${JSON.stringify(test)}],setupFiles:[...original.test.setupFiles,${JSON.stringify(hook)}],maxWorkers:1,fileParallelism:false}};`);
const summaries=[];let passed=false;
try{
 for(const retention of ["600000","0"]){
  const report=join(temporary,`report-${retention}.json`),stats=join(temporary,`native-${retention}.json`);
  const code=await new Promise((done,reject)=>{const child=spawn(process.execPath,[join(reference,"node_modules/vitest/vitest.mjs"),"run","--config",config,"--root",join(reference,"packages/durable"),"--reporter=json",`--outputFile=${report}`],{cwd:join(reference,"packages/durable"),env:{...process.env,PI_DURABLE_NATIVE_GENERATION_TASK:"1",PI_DURABLE_NATIVE_TOOL_TASK:"1",PI_DURABLE_NATIVE_COMPACTION_TASK:"1",PI_DURABLE_NATIVE_TASK_BRIDGE:join(target,profile,"examples/task_stock_bridge"),PI_DURABLE_NATIVE_TASK_STATS:stats,PI_DURABLE_DOCUMENT_RETENTION_MS:retention},stdio:"inherit"});child.once("error",reject);child.once("exit",done);});
  const result=JSON.parse(await readFile(report,"utf8"));
  if(code!==0||!result.success){for(const suite of result.testResults??[]){if(suite.message)console.error(suite.message);for(const test of suite.assertionResults??[])if(test.status==="failed")console.error(JSON.stringify(test,null,2));}throw new Error("Stock compaction tests failed against Rust");}
  assert.equal(result.numTotalTests,108,"Unexpected stock compaction test inventory");assert.equal(result.numPassedTests,result.numTotalTests,"Stock compaction tests were skipped");const native=JSON.parse(await readFile(stats,"utf8"));assert.ok(native.compactionReads>0&&native.nativeModelCompletions>0&&native.nativeCompactionHooks>0&&native.nativeModelRequests>0,"Native compaction or generation was bypassed");assert.equal(native.sourceGenerationPhases,0);assert.equal(native.sourceCompactionPhases,0);
  console.log(`Stock harness-compaction.test.ts retention=${retention}: ${result.numPassedTests}/${result.numTotalTests} tests passed; native=${JSON.stringify(native)}`);
  summaries.push({retention,passed:result.numPassedTests,total:result.numTotalTests,unselected:result.numTotalTests-result.numPassedTests,native,tests:result.testResults.flatMap(suite=>suite.assertionResults.map(test=>({name:test.fullName,status:test.status,durationMs:test.duration})))});
 }
 assert.deepEqual(await readFile(test),before,"Stock compaction test file was modified");
 const report=process.argv.find(arg=>arg.startsWith("--report="))?.slice(9);if(report)await writeFile(resolve(report),JSON.stringify({reference:pin,test:"packages/durable/test/harness-compaction.test.ts",sha256:createHash("sha256").update(before).digest("hex"),profile,nativeScope:"native compaction/generation/tool tasks, hooks, model collection dispatch, Session, scheduler, registry, plain memory, conversation views/events, range selection and serialization; original model collections, tool callbacks, schema validation, environments and controlled/persistent storage capabilities; source helper exports remain available",modes:summaries},null,2)+"\n");passed=true;
}finally{if(passed)await rm(temporary,{recursive:true,force:true});else console.error(`Failure artifacts retained: ${temporary}`);}
