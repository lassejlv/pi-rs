// Execute the complete unchanged upstream environment codec tests in native Rust.
import assert from "node:assert/strict";
import {execFileSync,spawn} from "node:child_process";
import {mkdtemp,readFile,writeFile,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {resolve,join} from "node:path";
import {fileURLToPath} from "node:url";
import {createHash} from "node:crypto";
const root=fileURLToPath(new URL("../",import.meta.url));
const reference=resolve(process.argv[2]??"../pi-mono"),pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);
const relative="packages/durable/test/env-line-scan.test.ts",test=join(reference,relative),before=await readFile(test);
assert.deepEqual(before,execFileSync("git",["show",`${pin}:${relative}`],{cwd:reference}),"Stock codec tests differ from pin");
execFileSync("cargo",["build","--locked","-q","--release","-p","pi-durable-codec-stock","--target","wasm32-unknown-unknown"],{cwd:root,stdio:"inherit"});
const target=process.env.CARGO_TARGET_DIR??JSON.parse(execFileSync("cargo",["metadata","--no-deps","--format-version","1"],{cwd:root,encoding:"utf8"})).target_directory;
const temporary=await mkdtemp(join(tmpdir(),"pi-durable-stock-codecs-"));
const adapter=fileURLToPath(new URL("stock/durable-codec-adapter.mjs",import.meta.url));
const setup=join(temporary,"setup.mjs"),config=join(temporary,"vitest.config.mjs");
await writeFile(setup,`import {afterAll} from ${JSON.stringify(join(reference,"node_modules/vitest/dist/index.js"))};import {writeFileSync} from "node:fs";import {counters} from ${JSON.stringify(adapter)};afterAll(()=>writeFileSync(process.env.PI_DURABLE_CODEC_STATS,JSON.stringify(counters())));`);
await writeFile(config,`import original from ${JSON.stringify(join(reference,"packages/durable/vitest.config.ts"))};import {resolve,dirname} from "node:path";
export default {...original,plugins:[{name:"native-rust-env-codecs",enforce:"pre",resolveId(source,importer){if(importer&&[${JSON.stringify(join(reference,"packages/durable/src/env/decode.ts"))},${JSON.stringify(join(reference,"packages/durable/src/env/line-scan.ts"))}].includes(resolve(dirname(importer),source)))return ${JSON.stringify(adapter)};}}],test:{...original.test,include:[${JSON.stringify(test)}],setupFiles:[...original.test.setupFiles,${JSON.stringify(setup)}],maxWorkers:1,fileParallelism:false}};`);
let passed=false;const modes=[];
try{
 for(const retention of ["600000","0"]){
  const report=join(temporary,`report-${retention}.json`),stats=join(temporary,`stats-${retention}.json`);
  const code=await new Promise((done,reject)=>{const child=spawn(process.execPath,[join(reference,"node_modules/vitest/vitest.mjs"),"run","--config",config,"--root",join(reference,"packages/durable"),"--reporter=json",`--outputFile=${report}`],{cwd:join(reference,"packages/durable"),env:{...process.env,PI_DURABLE_CODEC_WASM:join(target,"wasm32-unknown-unknown/release/pi_durable_codec_stock.wasm"),PI_DURABLE_CODEC_STATS:stats,PI_DURABLE_DOCUMENT_RETENTION_MS:retention},stdio:"inherit"});child.once("error",reject);child.once("exit",done);});
  const result=JSON.parse(await readFile(report,"utf8"));
  if(code!==0||!result.success){for(const suite of result.testResults??[])for(const item of suite.assertionResults??[])if(item.status==="failed")console.error(JSON.stringify(item,null,2));throw new Error("Stock environment codec tests failed against Rust");}
  assert.equal(result.numPendingTests,0);assert.equal(result.numPassedTests,4);
  const native=JSON.parse(await readFile(stats,"utf8"));assert.ok(native.decodes>10000);assert.equal(native.scans,5000);
  console.log(`Stock env-line-scan.test.ts retention=${retention}: ${result.numPassedTests}/4 tests passed; native=${JSON.stringify(native)}`);
  modes.push({retention,passed:result.numPassedTests,native,tests:result.testResults.flatMap(suite=>suite.assertionResults.map(item=>({name:item.fullName,status:item.status,durationMs:item.duration})))});
 }
 assert.deepEqual(await readFile(test),before);
 const report=process.argv.find(arg=>arg.startsWith("--report="))?.slice(9);if(report)await writeFile(resolve(report),JSON.stringify({reference:pin,test:relative,sha256:createHash("sha256").update(before).digest("hex"),nativeScope:"production Rust UTF-8 decoder and LineScanner compiled to WebAssembly; original generators, assertions and timing limits",modes},null,2)+"\n");
 passed=true;
}finally{if(passed)await rm(temporary,{recursive:true,force:true});else console.error(`Failure artifacts retained: ${temporary}`);}
