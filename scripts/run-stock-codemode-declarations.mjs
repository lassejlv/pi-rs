// Execute the complete pinned SDK declaration file against the native renderer.
import assert from "node:assert/strict";
import {execFileSync,spawn} from "node:child_process";
import {mkdtemp,readFile,writeFile,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {resolve,join} from "node:path";
import {fileURLToPath} from "node:url";
import {createHash} from "node:crypto";
const root=fileURLToPath(new URL("../",import.meta.url)),reference=resolve(process.argv[2]??"../pi-mono"),pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);
const path="packages/codemode/test/declarations.test.ts",test=join(reference,path),before=await readFile(test);
assert.deepEqual(before,execFileSync("git",["show",`${pin}:${path}`],{cwd:reference}));
execFileSync("cargo",["build","--locked","-q","-p","pi-codemode","--example","declarations_stock_bridge"],{cwd:root,stdio:"inherit"});
const target=process.env.CARGO_TARGET_DIR??JSON.parse(execFileSync("cargo",["metadata","--no-deps","--format-version","1"],{cwd:root,encoding:"utf8"})).target_directory;
const temporary=await mkdtemp(join(tmpdir(),"pi-durable-stock-declarations-")),adapter=fileURLToPath(new URL("stock/codemode-declarations-adapter.mjs",import.meta.url)),transport=fileURLToPath(new URL("stock/durable-native-transport.mjs",import.meta.url));
const hook=join(temporary,"setup.mjs"),config=join(temporary,"vitest.config.mjs");
await writeFile(hook,`import {afterAll} from ${JSON.stringify(join(reference,"node_modules/vitest/dist/index.js"))};import {writeFileSync} from "node:fs";import {nativeTransport} from ${JSON.stringify(transport)};if(await nativeTransport.request("transport.ready")!==true)throw new Error("Declaration bridge not ready");afterAll(async()=>{const calls=nativeTransport.sync("stats");if(calls===0)throw new Error("Stock tests bypassed Rust");await nativeTransport.close();writeFileSync(process.env.PI_CODEMODE_DECLARATION_STATS,JSON.stringify({calls}));});`);
await writeFile(config,`import original from ${JSON.stringify(join(reference,"packages/codemode/vitest.config.ts"))};import {resolve,dirname} from "node:path";export default {...original,plugins:[{name:"native-declarations",enforce:"pre",resolveId(source,importer){if(importer&&resolve(dirname(importer),source)===${JSON.stringify(join(reference,"packages/codemode/src/index.ts"))})return ${JSON.stringify(adapter)};}}],test:{...original.test,include:[${JSON.stringify(test)}],setupFiles:[${JSON.stringify(hook)}],maxWorkers:1,fileParallelism:false}};`);
const modes=[];let passed=false;
try {
 for(const retention of ["600000","0"]){
  const report=join(temporary,`report-${retention}.json`),stats=join(temporary,`stats-${retention}.json`);
  const code=await new Promise((done,reject)=>{const child=spawn(process.execPath,[join(reference,"node_modules/vitest/vitest.mjs"),"run","--config",config,"--root",join(reference,"packages/codemode"),"--reporter=json",`--outputFile=${report}`],{cwd:join(reference,"packages/codemode"),env:{...process.env,PI_DURABLE_NATIVE_TASK_BRIDGE:join(target,"debug/examples/declarations_stock_bridge"),PI_CODEMODE_DECLARATION_STATS:stats,PI_DURABLE_DOCUMENT_RETENTION_MS:retention},stdio:"inherit",detached:process.platform!=="win32"});const timer=setTimeout(()=>{if(process.platform==="win32")child.kill("SIGTERM");else process.kill(-child.pid,"SIGTERM");},120000);child.once("error",e=>{clearTimeout(timer);reject(e);});child.once("exit",code=>{clearTimeout(timer);done(code);});});
  const result=JSON.parse(await readFile(report,"utf8"));
  if(code!==0||!result.success){console.error(JSON.stringify(result,null,2));throw new Error("Stock declarations failed against Rust");}
  assert.equal(result.numPendingTests,0);assert.equal(result.numPassedTests,result.numTotalTests);
  const native=JSON.parse(await readFile(stats,"utf8"));assert.ok(native.calls>0);
  console.log(`Stock codemode/declarations.test.ts retention=${retention}: ${result.numPassedTests}/${result.numTotalTests} tests passed; native=${JSON.stringify(native)}`);
  modes.push({retention,passed:result.numPassedTests,native,tests:result.testResults.flatMap(file=>file.assertionResults.map(test=>({name:test.fullName,status:test.status,durationMs:test.duration})))});
 }
 assert.deepEqual(await readFile(test),before,"Pinned stock declarations changed");
 const report=process.argv.find(arg=>arg.startsWith("--report="))?.slice(9);
 if(report)await writeFile(resolve(report),JSON.stringify({reference:pin,test:path,sha256:createHash("sha256").update(before).digest("hex"),selection:"complete file without name filter",nativeScope:"all schema, signature, sample and declaration rendering executes in native Rust",modes},null,2)+"\n");
 passed=true;
}finally{if(passed)await rm(temporary,{recursive:true,force:true});else console.error(`Failure artifacts retained: ${temporary}`);}
