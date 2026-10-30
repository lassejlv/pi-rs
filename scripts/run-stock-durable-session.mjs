// Run complete, pinned upstream Session files against the native kernel.
import assert from "node:assert/strict";
import {execFileSync,spawn} from "node:child_process";
import {mkdtemp,readFile,writeFile,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {resolve,join} from "node:path";
import {fileURLToPath} from "node:url";
import {createHash} from "node:crypto";
const root=fileURLToPath(new URL("../",import.meta.url)),reference=resolve(process.argv[2]??"../pi-mono");
const pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);
const suite=process.argv.find(arg=>arg.startsWith("--suite="))?.slice(8)??"definitions";
assert.ok(["definitions","tables","documents","adoption","forks","watches","checkpoints-migrations","document-retention"].includes(suite));
const mixed=suite==="document-retention";
const filename=`session-${suite}.test.ts`,test=join(reference,"packages/durable/test",filename),before=await readFile(test);
assert.deepEqual(before,execFileSync("git",["show",`${pin}:packages/durable/test/${filename}`],{cwd:reference}));
execFileSync("cargo",["build","--locked","-q","-p","pi-durable",...(suite==="adoption"?["--features","stock-test-probes"]:[]),"--example","task_stock_bridge"],{cwd:root,stdio:"inherit"});
const target=process.env.CARGO_TARGET_DIR??JSON.parse(execFileSync("cargo",["metadata","--no-deps","--format-version","1"],{cwd:root,encoding:"utf8"})).target_directory;
const temporary=await mkdtemp(join(tmpdir(),"pi-durable-stock-session-")),adapter=fileURLToPath(new URL("stock/durable-task-adapter.mjs",import.meta.url));
const barrel=join(temporary,"barrel.mjs"),hook=join(temporary,"setup.mjs"),config=join(temporary,"vitest.config.mjs");
await writeFile(barrel,`export * from ${JSON.stringify(join(reference,"packages/durable/src/index.ts"))};import {MemoryStorage as SourceMemoryStorage} from ${JSON.stringify(join(reference,"packages/durable/src/storage/memory.ts"))};import {createNativeMemoryAdapter} from ${JSON.stringify(fileURLToPath(new URL("stock/durable-memory-adapter.mjs",import.meta.url)))};export const MemoryStorage=createNativeMemoryAdapter(SourceMemoryStorage);export {createSession} from ${JSON.stringify(fileURLToPath(new URL("stock/durable-session-adapter.mjs",import.meta.url)))};${mixed?`export {Harness,watchEvents} from ${JSON.stringify(adapter)};export {createRegistry} from ${JSON.stringify(fileURLToPath(new URL("stock/durable-registry-adapter.mjs",import.meta.url)))};`:""}`);
// Finish external worker/process startup in setup, before original test deadlines.
// The handshake creates no Session or Harness and exercises the same FIFO lane.
await writeFile(hook,`import {afterAll} from ${JSON.stringify(join(reference,"node_modules/vitest/dist/index.js"))};import {shutdownSessions} from ${JSON.stringify(adapter)};import {nativeTransport} from ${JSON.stringify(fileURLToPath(new URL("stock/durable-native-transport.mjs",import.meta.url)))};${mixed?`import {nativeTimer} from ${JSON.stringify(join(reference,"packages/durable/src/timers.ts"))};import {setDefaultSessionTimer} from ${JSON.stringify(fileURLToPath(new URL("stock/durable-session-adapter.mjs",import.meta.url)))};setDefaultSessionTimer(nativeTimer);`:""}if(await nativeTransport.request("transport.ready")!==true)throw new Error("Native Session bridge did not become ready");afterAll(shutdownSessions);`);
await writeFile(config,`import original from ${JSON.stringify(join(reference,"packages/durable/vitest.config.ts"))};import {existsSync} from "node:fs";import {resolve,dirname} from "node:path";
export default {...original,resolve:{...original.resolve,alias:original.resolve.alias.map(alias=>alias.find.test("@earendil-works/pi-durable")?{...alias,replacement:${JSON.stringify(barrel)}}:alias)},plugins:[{name:"native-session-kernel",enforce:"pre",resolveId(source,importer){
if(${JSON.stringify(suite==="adoption")}&&importer&&resolve(dirname(importer),source)===${JSON.stringify(join(reference,"packages/durable/src/session/transaction.ts"))})return ${JSON.stringify(fileURLToPath(new URL("stock/durable-transaction-probes.mjs",import.meta.url)))};
if(importer&&resolve(dirname(importer),source)===${JSON.stringify(join(reference,"packages/durable/src/session/session.ts"))})return ${JSON.stringify(fileURLToPath(new URL("stock/durable-session-adapter.mjs",import.meta.url)))};
const match=/^@earendil-works\\/(pi-ai|pi-telemetry|pi-codemode|chord)(?:\\/(.*))?$/.exec(source);
if(match){const path=${JSON.stringify(join(reference,"packages"))}+"/"+match[1].replace(/^pi-/,"")+"/src/"+(match[2]??"index");return [path+".ts",path+"/index.ts"].find(existsSync);}
}}],test:{...original.test,include:[${JSON.stringify(test)}],setupFiles:[...original.test.setupFiles,${JSON.stringify(hook)}],maxWorkers:1,fileParallelism:false}};`);
const modes=[];let passed=false;
try{
 for(const retention of ["600000","0"]){
  const report=join(temporary,`report-${retention}.json`),stats=join(temporary,`stats-${retention}.json`);
  const code=await new Promise((done,reject)=>{const child=spawn(process.execPath,[join(reference,"node_modules/vitest/vitest.mjs"),"run","--config",config,"--root",join(reference,"packages/durable"),"--reporter=json",`--outputFile=${report}`],{cwd:join(reference,"packages/durable"),env:{...process.env,PI_DURABLE_NATIVE_TASK_BRIDGE:join(target,"debug/examples/task_stock_bridge"),PI_DURABLE_NATIVE_TASK_STATS:stats,PI_DURABLE_DOCUMENT_RETENTION_MS:retention,PI_DURABLE_NATIVE_SESSION_HARNESS:mixed?"1":"0",PI_DURABLE_NATIVE_TRANSACTION_PROBES:suite==="adoption"?"1":"0",PI_DURABLE_NATIVE_GENERATION_TASK:mixed?"1":"0",PI_DURABLE_NATIVE_COMPACTION_TASK:mixed?"1":"0",PI_DURABLE_NATIVE_TOOL_TASK:mixed?"1":"0"},stdio:"inherit",detached:process.platform!=="win32"});const timeout=setTimeout(()=>{console.error("Stock Session process exceeded the 120-second outer limit");if(process.platform==="win32")child.kill("SIGTERM");else process.kill(-child.pid,"SIGTERM");},120_000);child.once("error",error=>{clearTimeout(timeout);reject(error);});child.once("exit",code=>{clearTimeout(timeout);done(code);});});
  const result=JSON.parse(await readFile(report,"utf8"));
  if(code!==0||!result.success){for(const file of result.testResults??[]){if(file.message)console.error(file.message);for(const test of file.assertionResults??[])if(test.status==="failed")console.error(JSON.stringify(test,null,2));}throw new Error("Stock Session tests failed against Rust");}
  assert.equal(result.numPendingTests,0);assert.equal(result.numPassedTests,result.numTotalTests);
  const native=JSON.parse(await readFile(stats,"utf8"));assert.ok(native.nativeSessionOpens>0);if(mixed){assert.ok(native.nativeHarnessOpens>0&&native.nativeModelRequests>0&&native.nativeToolCalls>0);}else assert.equal(native.nativeHarnessOpens,0);assert.equal(native.phases,0);
  console.log(`Stock ${filename} retention=${retention}: ${result.numPassedTests}/${result.numTotalTests} tests passed; native=${JSON.stringify(native)}`);
  modes.push({retention,passed:result.numPassedTests,native,tests:result.testResults.flatMap(file=>file.assertionResults.map(test=>({name:test.fullName,status:test.status,durationMs:test.duration})))});
 }
 assert.deepEqual(await readFile(test),before,"Pinned stock test changed");
 const report=process.argv.find(arg=>arg.startsWith("--report="))?.slice(9);
 if(report)await writeFile(resolve(report),JSON.stringify({reference:pin,test:`packages/durable/test/${filename}`,sha256:createHash("sha256").update(before).digest("hex"),selection:"complete file without name filter",nativeScope:(suite==="adoption"?"original transaction prototype spies and injected failures are test-only native settlement probes under stock-test-probes; actual adoption/discard remains Rust; late backend monkeypatches are forwarded through host storage capabilities; ":"")+(mixed?"native Harness, generation, tools and events in the mixed retention cases; original host timer primitives remain capabilities; ":"")+"native Session kernel, document cache, transactions and plain memory backend; original controlled storage, definition callbacks, strict JS input copying, mutable draft capture, arbitrary callback results and table-record identities remain host capabilities; document and operation identities come from live native owners, while a complete allocation audit remains open; direct helper and compile-time type assertions remain source",modes},null,2)+"\n");
 passed=true;
}finally{if(passed)await rm(temporary,{recursive:true,force:true});else console.error(`Failure artifacts retained: ${temporary}`);}
