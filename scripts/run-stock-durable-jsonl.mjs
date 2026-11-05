// Run the pinned, unmodified pi-mono jsonl-storage.test.ts against Rust.
// Vite substitutes only the JSONL implementation; its test code, assertions,
// filesystem and injected failures are all the originals.
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
const test=join(reference,"packages/durable/test/jsonl-storage.test.ts");
const before=await readFile(test);
assert.deepEqual(before,execFileSync("git",["show",`${pin}:packages/durable/test/jsonl-storage.test.ts`],{cwd:reference}),"Stock JSONL tests differ from the pinned version");
execFileSync("cargo",["build","--locked","-q","-p","pi-durable","--example","jsonl_stock_bridge"],{cwd:root,stdio:"inherit"});
const target=process.env.CARGO_TARGET_DIR??JSON.parse(execFileSync("cargo",["metadata","--no-deps","--format-version","1"],{cwd:root,encoding:"utf8"})).target_directory;
const binary=join(target,"debug/examples/jsonl_stock_bridge");
const temporary=await mkdtemp(join(tmpdir(),"pi-durable-stock-"));
const adapter=fileURLToPath(new URL("stock/durable-jsonl-adapter.mjs",import.meta.url));
const hook=join(temporary,"setup.mjs");
await writeFile(hook,`import {afterAll} from ${JSON.stringify(join(reference,"node_modules/vitest/dist/index.js"))};\nimport {shutdown} from ${JSON.stringify(adapter)};\nafterAll(shutdown,60000);\n`);
const config=join(temporary,"vitest.config.mjs");
await writeFile(config,`import original from ${JSON.stringify(join(reference,"packages/durable/vitest.config.ts"))};
export default {...original,plugins:[{name:"rust-jsonl-under-stock-tests",enforce:"pre",resolveId(source){
 if(/(?:^|\\/)storage\\/jsonl\\/(?:index|storage)\\.ts$/.test(source))return "\\0rust-jsonl";
 if(/(?:^|\\/)storage\\/jsonl\\/node\\.ts$/.test(source))return "\\0rust-jsonl-node";
},load(id){
 if(id==="\\0rust-jsonl")return ${JSON.stringify(`export {JsonlStorage,JsonlCorruptionError,JsonlStoragePoisonedError} from ${JSON.stringify(adapter)};`)};
 if(id==="\\0rust-jsonl-node")return ${JSON.stringify(`import {JsonlStorage} from ${JSON.stringify(adapter)};import {NodeExecutionEnv} from ${JSON.stringify(join(reference,"packages/durable/src/env/node.ts"))};export {JsonlStorage};export const openNodeJsonlStorage=(directory,context,options={})=>JsonlStorage.open(directory,new NodeExecutionEnv({cwd:process.cwd()}),context,options);`)};
}}],test:{...original.test,include:[${JSON.stringify(test)}],setupFiles:[...original.test.setupFiles,${JSON.stringify(hook)}],maxWorkers:1,fileParallelism:false}};\n`);
let passed=false;
const summaries=[];
try{
 for(const retention of ["600000","0"]){
  const report=join(temporary,`report-${retention}.json`);
  const statsPath=join(temporary,`native-${retention}.json`);
  const filter=process.env.PI_DURABLE_STOCK_TEST_FILTER?["-t",process.env.PI_DURABLE_STOCK_TEST_FILTER]:[];
  const code=await new Promise((resolveExit,reject)=>{const child=spawn(process.execPath,[join(reference,"node_modules/vitest/vitest.mjs"),"run","--config",config,"--root",join(reference,"packages/durable"),"--reporter=json",`--outputFile=${report}`,...filter],{cwd:join(reference,"packages/durable"),env:{...process.env,PI_DURABLE_NATIVE_JSONL_BRIDGE:binary,PI_DURABLE_NATIVE_JSONL_STATS:statsPath,PI_DURABLE_DOCUMENT_RETENTION_MS:retention},stdio:"inherit"});child.once("error",reject);child.once("exit",resolveExit);});
  const result=JSON.parse(await readFile(report,"utf8"));
  console.log(`Stock jsonl-storage.test.ts retention=${retention}: ${result.numPassedTests}/${result.numTotalTests} tests passed`);
  if(code!==0||!result.success){for(const suite of result.testResults??[])for(const test of suite.assertionResults??[])if(test.status==="failed")console.error(JSON.stringify(test,null,2));throw new Error("Stock JSONL tests failed against Rust");}
  const stats=JSON.parse(await readFile(statsPath,"utf8"));assert.ok(stats.opens>0&&stats.commits>0&&stats.adoptions>0&&stats.filesystemCalls>0,"Native backend was bypassed");
  console.log(`Verified native backend calls: ${JSON.stringify(stats)}`);
  if(!filter.length)assert.equal(result.numPassedTests,result.numTotalTests,"Stock tests were skipped");
  summaries.push({retention,passed:result.numPassedTests,total:result.numTotalTests,native:stats,tests:result.testResults.flatMap(s=>s.assertionResults.map(t=>({name:t.fullName,status:t.status,durationMs:t.duration})))});
 }
 assert.deepEqual(await readFile(test),before,"Stock test file was modified");
 passed=true;
 const report=process.argv.find(a=>a.startsWith("--report="))?.slice("--report=".length);
 if(report)await writeFile(resolve(report),JSON.stringify({reference:pin,test:"packages/durable/test/jsonl-storage.test.ts",sha256:createHash("sha256").update(before).digest("hex"),filter:process.env.PI_DURABLE_STOCK_TEST_FILTER??null,modes:summaries},null,2)+"\n");
}finally{if(passed)await rm(temporary,{recursive:true,force:true});else console.error(`Failure artifacts retained: ${temporary}`);}
