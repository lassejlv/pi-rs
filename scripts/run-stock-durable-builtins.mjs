// Run the original mutation-tool assertions over native implementations.
import assert from "node:assert/strict";
import {execFileSync,spawn} from "node:child_process";
import {mkdtemp,readFile,writeFile,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {resolve,join} from "node:path";
import {fileURLToPath,pathToFileURL} from "node:url";
import {createHash} from "node:crypto";
const root=fileURLToPath(new URL("../",import.meta.url)),reference=resolve(process.argv[2]??"../pi-mono"),pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);
const suite=process.argv.find(arg=>arg.startsWith("--suite="))?.slice(8)??"all";assert.ok(["all","mutations","read-differential","images"].includes(suite));
const filename=suite==="read-differential"?"tools-read-differential.test.ts":suite==="images"?"tools-images.test.ts":"tools.test.ts",relative=`packages/durable/test/${filename}`,test=join(reference,relative),before=await readFile(test);
assert.deepEqual(before,execFileSync("git",["show",`${pin}:${relative}`],{cwd:reference}));
execFileSync("cargo",["build","--locked","-q","-p","pi-durable","--example","task_stock_bridge"],{cwd:root,stdio:"inherit"});
const target=process.env.CARGO_TARGET_DIR??JSON.parse(execFileSync("cargo",["metadata","--no-deps","--format-version","1"],{cwd:root,encoding:"utf8"})).target_directory;
const temporary=await mkdtemp(join(tmpdir(),"pi-durable-stock-builtins-")),adapter=fileURLToPath(new URL("stock/durable-builtin-adapter.mjs",import.meta.url));
const config=join(temporary,"vitest.config.mjs"),setup=join(temporary,"setup.mjs");
const childHook=join(temporary,"child-loader.mjs"),childAdapter=fileURLToPath(new URL("stock/durable-base64-child.mjs",import.meta.url));
await writeFile(childHook,`import {registerHooks} from "node:module";if(process.execArgv.includes("--max-heap-size=128"))registerHooks({resolve(specifier,context,next){const value=next(specifier,context);return value.url===${JSON.stringify(pathToFileURL(join(reference,"packages/durable/src/tools/image-processor.ts")).href)}?{url:${JSON.stringify(pathToFileURL(childAdapter).href)},shortCircuit:true}:value;}});`);

await writeFile(setup,`import {afterAll} from ${JSON.stringify(join(reference,"node_modules/vitest/dist/index.js"))};import {writeFileSync} from "node:fs";afterAll(async()=>{const {shutdown}=await import(${JSON.stringify(adapter)});writeFileSync(process.env.PI_DURABLE_BUILTIN_STATS,JSON.stringify(await shutdown()));},60000);`);
const redirects=["write.ts","edit.ts","file-mutation-queue.ts","read.ts","bash.ts","image.ts","image-processor.ts"].map(name=>join(reference,"packages/durable/src/tools",name));redirects.push(...["node.ts","index.ts","exif.ts"].map(name=>join(reference,"packages/durable/src/images",name)));
await writeFile(config,`import original from ${JSON.stringify(join(reference,"packages/durable/vitest.config.ts"))};import {resolve,dirname} from "node:path";
const redirects=${JSON.stringify(redirects)};export default {...original,plugins:[{name:"native-rust-builtins",enforce:"pre",resolveId(source,importer){if(source==="@earendil-works/pi-durable/images"||source==="@earendil-works/pi-durable/images/node")return ${JSON.stringify(adapter)};if(source==="@earendil-works/chord/context")return ${JSON.stringify(join(reference,"packages/chord/src/context/index.ts"))};if(importer&&redirects.includes(resolve(dirname(importer),source))&&!(importer===${JSON.stringify(test)}&&${JSON.stringify(suite)}==="read-differential"&&source.endsWith("/image.ts")))return ${JSON.stringify(adapter)};}}],test:{...original.test,include:[${JSON.stringify(test)}],setupFiles:[...original.test.setupFiles,${JSON.stringify(setup)}],maxWorkers:1,fileParallelism:false}};`);
let passed=false;const modes=[];
try{
 for(const retention of ["600000","0"]){
  const report=join(temporary,`report-${retention}.json`),stats=join(temporary,`stats-${retention}.json`),base64Marker=join(temporary,`base64-${retention}.txt`);
  const code=await new Promise((done,reject)=>{const child=spawn(process.execPath,[join(reference,"node_modules/vitest/vitest.mjs"),"run","--config",config,"--root",join(reference,"packages/durable"),"--reporter=json",`--outputFile=${report}`,...(suite==="mutations"?["-t","^durable tools (write|edit) "]:[])],{cwd:join(reference,"packages/durable"),env:{...process.env,...(suite==="images"?{NODE_OPTIONS:[process.env.NODE_OPTIONS,`--import=${pathToFileURL(childHook).href}`].filter(Boolean).join(" "),PI_DURABLE_NATIVE_BASE64_MARKER:base64Marker}:{}),PI_DURABLE_NATIVE_TASK_BRIDGE:join(target,"debug/examples/task_stock_bridge"),PI_DURABLE_BUILTIN_STATS:stats,PI_DURABLE_DOCUMENT_RETENTION_MS:retention},stdio:"inherit"});child.once("error",reject);child.once("exit",done);});
  const result=JSON.parse(await readFile(report,"utf8"));
  if(code!==0||!result.success){for(const suite of result.testResults??[]){if(suite.message)console.error(suite.message);for(const item of suite.assertionResults??[])if(item.status==="failed")console.error(JSON.stringify(item,null,2));}throw new Error("Stock built-in tool tests failed against Rust");}
  const tests=result.testResults.flatMap(suite=>suite.assertionResults.filter(item=>item.status==="passed"));
  if(suite==="mutations"){
   const section=before.toString().split('\tdescribe("write",')[1].split('\tdescribe("powershell",')[0];
   const selected=[...section.matchAll(/\bit\("([^"\n]+)"/g)].map(match=>match[1]);if(process.platform!=="win32")selected.push("keys a missing file whose name contains a backslash like the created file");assert.deepEqual(tests.map(item=>item.title).sort(),selected.sort());
  }else{const total=suite==="all"?36:suite==="images"?34:[...before.toString().matchAll(/\bit(?:\.skipIf\([^)]*\)|\.runIf\([^)]*\))?\s*\(/g)].length;assert.equal(result.numTotalTests,total);assert.equal(result.numPendingTests,suite==="all"?1:0);assert.equal(tests.length,total-result.numPendingTests);}
  const native=JSON.parse(await readFile(stats,"utf8"));if(suite==="images"){assert.equal((await readFile(base64Marker,"utf8")).trim(),"3147295","Stock heap test bypassed native base64");native.nativeBase64Subprocesses=1;}assert.ok(native.environmentCalls>0);assert.equal(native.activeReaders,0);if(suite==="all"||suite==="mutations"){assert.ok(native.writes>0&&native.edits>0);if(process.platform!=="win32")assert.ok(native.queues>0&&native.functions>0);}if(suite!=="mutations")assert.ok(native.reads>0);if(suite==="all")assert.ok(native.shells>0);if(suite==="images")assert.ok(native.imageProcessors>0&&native.imagePreparations>0,"Native image processing was bypassed");
  console.log(`Stock ${filename} retention=${retention}: ${tests.length} selected cases passed, ${result.numPendingTests} not selected or original platform skips; native=${JSON.stringify(native)}`);
  modes.push({retention,passed:tests.length,notSelected:result.numPendingTests,native,tests:tests.map(item=>({name:item.fullName,status:item.status,durationMs:item.duration}))});
 }
 assert.deepEqual(await readFile(test),before);const report=process.argv.find(arg=>arg.startsWith("--report="))?.slice(9);if(report)await writeFile(resolve(report),JSON.stringify({reference:pin,test:relative,sha256:createHash("sha256").update(before).digest("hex"),nativeScope:suite==="mutations"?"native write/edit and canonical-path mutation admission; original environment subclasses and I/O gates":"native built-in read/write/edit/bash/PowerShell implementations, image signatures/dimensions/limits, progress and mutation admission; native Photon decoding/orientation/resize/encoding and image workers when selected; original environment implementations, model collections and prepare callbacks remain host capabilities",modes},null,2)+"\n");passed=true;
}finally{if(passed)await rm(temporary,{recursive:true,force:true});else console.error(`Failure artifacts retained: ${temporary}`);}
