// Run the pinned SQLite files with their original tests, assertions and deadlines.
import assert from "node:assert/strict";
import {execFileSync,spawn} from "node:child_process";
import {mkdtemp,writeFile,readFile,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {resolve,join} from "node:path";
import {fileURLToPath} from "node:url";
import {createHash} from "node:crypto";
const root=fileURLToPath(new URL("../",import.meta.url));
const reference=resolve(process.argv[2]??"../pi-mono"),pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);
const files=["sqlite-storage.test.ts","sqlite-migrations.test.ts","sqlite-facade.test.ts","sqlite-cloudflare.test.ts"];
const tests=files.map(name=>join(reference,"packages/durable/test",name));
const original=await Promise.all(tests.map(path=>readFile(path)));
for(let i=0;i<tests.length;i++)assert.deepEqual(original[i],execFileSync("git",["show",`${pin}:packages/durable/test/${files[i]}`],{cwd:reference}),`Stock tests modified: ${files[i]}`);
execFileSync("cargo",["build","--locked","-q","-p","pi-durable","--example","sqlite_stock_bridge"],{cwd:root,stdio:"inherit"});
const target=process.env.CARGO_TARGET_DIR??JSON.parse(execFileSync("cargo",["metadata","--no-deps","--format-version","1"],{cwd:root,encoding:"utf8"})).target_directory;
const binary=join(target,"debug/examples/sqlite_stock_bridge");
const temporary=await mkdtemp(join(tmpdir(),"pi-durable-stock-sqlite-"));
const adapter=fileURLToPath(new URL("stock/durable-sqlite-adapter.mjs",import.meta.url));
const hook=join(temporary,"setup.mjs");
await writeFile(hook,`import {afterAll} from ${JSON.stringify(join(reference,"node_modules/vitest/dist/index.js"))};import {shutdown} from ${JSON.stringify(adapter)};afterAll(shutdown,60000);\n`);
const nodeDriver=join(reference,"packages/chord/src/sqlite/node.ts"),cloudflareDriver=join(reference,"packages/chord/src/sqlite/cloudflare.ts");
const replacements={
 "\\0rust-sqlite":`export {SqliteStorage,SQLITE_MIGRATIONS,CURRENT_SQLITE_SCHEMA_VERSION,applySqliteMigrations,DEFAULT_SQLITE_SCHEMA} from ${JSON.stringify(adapter)};`,
 "\\0rust-sqlite-node":`export {openNodeSqliteStorage} from ${JSON.stringify(adapter)};export {NodeSqliteDatabase,openNodeSqliteDatabase} from ${JSON.stringify(nodeDriver)};`,
 "\\0rust-sqlite-cloudflare":`import {SqliteStorage} from ${JSON.stringify(adapter)};import {DurableObjectSqliteDatabase} from ${JSON.stringify(cloudflareDriver)};export {DurableObjectSqliteDatabase};export const openDurableObjectSqliteStorage=(storage,options={})=>SqliteStorage.open(new DurableObjectSqliteDatabase(storage),options);`,
};
// One Vitest process per file makes execution counters independently auditable.
const config=join(temporary,"vitest.config.mjs");
await writeFile(config,`import original from ${JSON.stringify(join(reference,"packages/durable/vitest.config.ts"))};
const replacements=${JSON.stringify(replacements)};
export default {...original,plugins:[{name:"native-sqlite-under-stock-tests",enforce:"pre",resolveId(source){
 if(/(?:^|\\/)storage\\/sqlite\\/(?:index|storage)\\.ts$/.test(source))return "\\0rust-sqlite";
 if(/(?:^|\\/)storage\\/sqlite\\/node\\.ts$/.test(source))return "\\0rust-sqlite-node";
 if(/(?:^|\\/)storage\\/sqlite\\/cloudflare(?:\\.ts)?$/.test(source))return "\\0rust-sqlite-cloudflare";
},load(id){return replacements[id.replaceAll("\\0","\\\\0")]??null;}}],test:{...original.test,include:${JSON.stringify(tests)},setupFiles:[...original.test.setupFiles,${JSON.stringify(hook)}],maxWorkers:1,fileParallelism:false}};\n`);
let passed=false;const modes=[];
try{
 for(const retention of ["600000","0"]){
  const reports=[];
  for(let i=0;i<tests.length;i++){
   const report=join(temporary,`${files[i]}-${retention}.json`),statsPath=join(temporary,`native-${files[i]}-${retention}.json`);
   const filter=process.env.PI_DURABLE_STOCK_TEST_FILTER?["-t",process.env.PI_DURABLE_STOCK_TEST_FILTER]:[];
   const code=await new Promise((resolveExit,reject)=>{
    const child=spawn(process.execPath,[join(reference,"node_modules/vitest/vitest.mjs"),"run",tests[i],"--config",config,"--root",join(reference,"packages/durable"),"--reporter=json",`--outputFile=${report}`,...filter],{cwd:join(reference,"packages/durable"),env:{...process.env,PI_DURABLE_NATIVE_TASK_BRIDGE:binary,PI_DURABLE_NATIVE_SQLITE_STATS:statsPath,PI_DURABLE_DOCUMENT_RETENTION_MS:retention},stdio:"inherit"});
    child.once("error",reject);child.once("exit",resolveExit);
   });
   const result=JSON.parse(await readFile(report,"utf8"));
   console.log(`Stock ${files[i]} retention=${retention}: ${result.numPassedTests}/${result.numTotalTests} passed`);
   if(code!==0||!result.success){for(const suite of result.testResults??[])for(const test of suite.assertionResults??[])if(test.status==="failed")console.error(JSON.stringify(test,null,2));throw new Error(`Stock SQLite tests failed: ${files[i]}`);}
   const stats=JSON.parse(await readFile(statsPath,"utf8"));assert.ok(stats.opens>0&&stats.commits>0&&stats.admitted>0,"Native storage was bypassed");
   if(!filter.length)assert.equal(result.numPassedTests,result.numTotalTests,"Stock tests skipped");
   reports.push({file:files[i],passed:result.numPassedTests,total:result.numTotalTests,native:stats,tests:result.testResults.flatMap(s=>s.assertionResults.map(t=>({name:t.fullName,status:t.status,durationMs:t.duration})))});
  }
  modes.push({retention,reports});
 }
 for(let i=0;i<tests.length;i++)assert.deepEqual(await readFile(tests[i]),original[i],"Stock test was modified");
 const report=process.argv.find(arg=>arg.startsWith("--report="))?.slice(9);
 if(report)await writeFile(resolve(report),JSON.stringify({reference:pin,files:files.map((file,i)=>({file,sha256:createHash("sha256").update(original[i]).digest("hex")})),filter:process.env.PI_DURABLE_STOCK_TEST_FILTER??null,modes},null,2)+"\n");
 passed=true;
}finally{if(passed)await rm(temporary,{recursive:true,force:true});else console.error(`Failure artifacts: ${temporary}`);}
