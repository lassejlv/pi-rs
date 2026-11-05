// Run the unmodified reference storage suite, capturing independent backend traces.
// Optionally run the same assertions against a native Rust backend over JSON lines.
import assert from "node:assert/strict";
import {spawn,execFileSync} from "node:child_process";
import {readFile,writeFile,mkdir,mkdtemp,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {resolve,dirname} from "node:path";
import {pathToFileURL,fileURLToPath} from "node:url";
import {createInterface} from "node:readline";

const pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
const reference=resolve(process.argv[2]??"../pi-mono");
if(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim()!==pin) throw new Error("Reference differs from pin");
const {MemoryStorage}=await import(pathToFileURL(resolve(reference,"packages/durable/src/storage/memory.ts")));
const {createStorageConformance}=await import(pathToFileURL(resolve(reference,"packages/durable/src/testing/storage-conformance.ts")));
const {BACKGROUND_CONTEXT}=await import(pathToFileURL(resolve(reference,"packages/chord/src/context/index.ts")));
const jsonl=process.argv.includes("--jsonl");
const sqlite=process.argv.includes("--sqlite");
if(jsonl&&sqlite)throw new Error("Choose one persistence backend");
const reopen=process.argv.includes("--reopen");
if(reopen&&!jsonl&&!sqlite)throw new Error("--reopen requires --jsonl or --sqlite");
const backendName=(jsonl||sqlite)?`${sqlite?"sqlite":"jsonl"}${reopen?"-reopen":""}`:"memory";
let JsonlStorage,NodeExecutionEnv,openSqlite;
if(sqlite)({openNodeSqliteStorage:openSqlite}=await import(pathToFileURL(resolve(reference,"packages/durable/src/storage/sqlite/node.ts"))));
if(jsonl){({JsonlStorage}=await import(pathToFileURL(resolve(reference,"packages/durable/src/storage/jsonl/storage.ts"))));({NodeExecutionEnv}=await import(pathToFileURL(resolve(reference,"packages/durable/src/env/node.ts"))));}
const assertions={
 ok:assert.ok,strictEqual:assert.strictEqual,deepEqual:assert.deepStrictEqual,partialDeepEqual:assert.partialDeepStrictEqual,
 greaterThan(actual,expected){assert.ok(actual>expected,`${actual} should exceed ${expected}`);},
 async rejects(operation,includes){await assert.rejects(operation,error=>error instanceof Error && error.message.includes(includes));},
};
const arities={commit:1,mintId:0,conversation:1,scanConversations:3,entry:null,findLatestHeadMarker:2,scanEntries:3,task:1,scanTasks:3,submission:1,scanSubmissions:3,submissionByRequest:2,findDocument:2,document:2,scanDocuments:3,close:0};
const detach=value=>value===undefined?undefined:JSON.parse(JSON.stringify(value));
const records=[];let current;
function instrument(storage){return new Proxy(storage,{get(target,name){
 const method=target[name];if(typeof method!=="function" || !(name in arities)) return method;
 return async (...args)=>{
  const length=name==="entry"?args.length-1:arities[name];
  const record={method:name,args:detach(args.slice(0,length)),adopted:[]};current.push(record);
  try {
   const callArgs=[...args];
   if(name==="commit"){const original=args[1];callArgs[1]=seq=>{record.adopted.push(seq);original(seq);};}
   const result=await method.apply(target,callArgs);
   if(result===undefined) record.missing=true;else record.value=detach(result);
   return result;
  }catch(error){record.error={message:error.message,name:error.name};throw error;}
 };
}});}
const referenceCases=createStorageConformance({assertions,withStorage:async use=>{
 if(!jsonl&&!sqlite){const storage=new MemoryStorage();await use(instrument(storage));await storage.close(BACKGROUND_CONTEXT);return;}
 const directory=await mkdtemp(resolve(tmpdir(),`pi-durable-${backendName}-reference-`));
 const fs=jsonl?new NodeExecutionEnv({cwd:directory}):undefined;const open=()=>sqlite?openSqlite(resolve(directory,"storage.sqlite")):JsonlStorage.open(directory,fs,BACKGROUND_CONTEXT);let current=await open();let closed=false;
 const storage=reopen?new Proxy({}, {get(_,name){return async (...args)=>{
  if(name==="close"){closed=true;return current.close(...args);}
  if(name!=="commit"||closed)return current[name](...args);
  try{return await current.commit(...args);}finally{await current.close(BACKGROUND_CONTEXT);current=await open();}
 };}}):current;
 try{await use(instrument(storage));}finally{await current.close(BACKGROUND_CONTEXT);await rm(directory,{recursive:true,force:true});}
}});
for(const test of referenceCases){current=[];await test.run();records.push({name:test.name,requests:current});}
const fixtureName=(jsonl||sqlite)?`storage-${backendName}`:"storage";
const target=fileURLToPath(new URL(`../../crates/pi-durable/tests/fixtures/${fixtureName}.json`,import.meta.url));
const output=JSON.stringify({reference:pin,cases:records},null,2)+"\n";
if(process.argv.includes("--check")){assert.equal(await readFile(target,"utf8"),output,"Storage traces differ from reference");}
else {await mkdir(dirname(target),{recursive:true});await writeFile(target,output);}
const rustTests=records.map((test,index)=>`storage_case!(${test.name.toLowerCase().replaceAll(/[^a-z0-9]+/g,"_").replaceAll(/^_|_$/g,"")}, ${index});`).join("\n")+"\n";
const rustTarget=fileURLToPath(new URL(`../../crates/pi-durable/tests/fixtures/${(jsonl||sqlite)?`storage_${backendName.replaceAll("-","_")}_cases`:"storage_cases"}.rs`,import.meta.url));
if(process.argv.includes("--check")){assert.equal(await readFile(rustTarget,"utf8"),rustTests,"Rust conformance case map differs");}else await writeFile(rustTarget,rustTests);
console.log(`Reference ${backendName} passed ${records.length} storage conformance cases with ${records.reduce((n,c)=>n+c.requests.length,0)} recorded operations`);

if(process.argv.includes("--native")||process.argv.includes("--interchange")){
 const root=fileURLToPath(new URL("../../",import.meta.url));
 const child=spawn("cargo",["run","--locked","-q","-p","pi-durable","--example","storage_conformance","--",...(jsonl?["--jsonl"]:[]),...(sqlite?["--sqlite"]:[]),...(reopen?["--reopen"]:[])],{cwd:root,stdio:["pipe","pipe","inherit"]});
 const pending=new Map();let next=0;let stderrError;let closing=false;
 const exited=new Promise(resolve=>{child.once("exit",resolve);child.once("error",()=>resolve(1));});
 const lines=createInterface({input:child.stdout});
 const fail=error=>{stderrError=error;for(const wait of pending.values())wait.reject(error);pending.clear();};
 child.stdin.on("error",fail);
 child.on("error",fail);child.on("exit",code=>{if(!closing||code!==0)fail(new Error(`Native conformance driver exited ${code}`));});
 lines.on("line",line=>{
  const frame=JSON.parse(line);const wait=pending.get(frame.id);if(!wait)return;
  if(frame.type==="adopt"){wait.adopt?.(frame.seq);return;}
  pending.delete(frame.id);
  if(frame.error){const error=new Error(frame.error.message);error.name=frame.error.name;wait.reject(error);}
  else wait.resolve(frame.missing?undefined:frame.value);
 });
 const call=(method,args,adopt)=>new Promise((resolve,reject)=>{
  if(stderrError){reject(stderrError);return;}
  const id=next++;pending.set(id,{resolve,reject,adopt});child.stdin.write(JSON.stringify({id,method,args})+"\n");
 });
 const storage=new Proxy({}, {get(_,name){return(...args)=>call(name,args.slice(0,name==="entry"?args.length-1:arities[name]),name==="commit"?args[1]:undefined);}});
 const cases=createStorageConformance({assertions,withStorage:async use=>{await call("reset",[]);await use(storage);}});
 try {
  if(process.argv.includes("--native")){for(const test of cases){await test.run();console.log(`Native: ${test.name}`);}console.log(`Native passed all ${cases.length} reference storage cases`);}
  if(process.argv.includes("--interchange")){
   if((!jsonl&&!sqlite)||reopen)throw new Error("--interchange requires --jsonl or --sqlite without --reopen");
   const hybridCases=createStorageConformance({assertions,withStorage:async use=>{
    await call("reset",[]);const directory=await call("directory",[]);const fs=jsonl?new NodeExecutionEnv({cwd:directory}):undefined;const open=()=>sqlite?openSqlite(directory):JsonlStorage.open(directory,fs,BACKGROUND_CONTEXT);
    let current=await open();let native=false;let closed=false;
    const hybrid=new Proxy({}, {get(_,name){return async (...args)=>{
     if(name==="close"){closed=true;return native?storage.close(...args):current.close(...args);}
     if(name!=="commit"||closed)return native?storage[name](...args):current[name](...args);
     try{return await (native?storage.commit(...args):current.commit(...args));}
     finally{
      if(native){await storage.close(BACKGROUND_CONTEXT);current=await open();}
      else {await current.close(BACKGROUND_CONTEXT);await call("reopen",[]);}
      native=!native;
     }
    };}});
    try{await use(hybrid);}finally{await current.close(BACKGROUND_CONTEXT);await storage.close(BACKGROUND_CONTEXT);}
   }});
   for(const test of hybridCases){await test.run();console.log(`Interchange: ${test.name}`);}
   console.log(`Rust/TypeScript alternating commits passed all ${hybridCases.length} reference storage cases`);
  }
 }
 finally {closing=true;child.stdin.end();const code=await exited;lines.close();if(code!==0)throw new Error(`Native conformance driver exited ${code}`);}
}
