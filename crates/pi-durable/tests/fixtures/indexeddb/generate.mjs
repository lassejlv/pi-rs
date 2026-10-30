// Record the reference IndexedDbStorage (over fake-indexeddb, as its own tests
// run it) for the native IndexedDB port:
//  - the unmodified storage conformance suite, plain and across reopen;
//  - an interchange scenario: its request trace plus every object store's rows.
//
//   node generate.mjs <pi-mono checkout> <fake-indexeddb package dir> [--check]
import assert from "node:assert/strict";
import {execFileSync} from "node:child_process";
import {readFile,writeFile} from "node:fs/promises";
import {resolve} from "node:path";
import {pathToFileURL,fileURLToPath} from "node:url";

const pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
const reference=resolve(process.argv[2]??"../pi-mono");
const fakeDir=resolve(process.argv[3]??"node_modules/fake-indexeddb");
if(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim()!==pin)throw new Error("Reference differs from pin");
const check=process.argv.includes("--check");
const source=path=>import(pathToFileURL(resolve(reference,path)));
const fake=await import(pathToFileURL(resolve(fakeDir,"build/esm/index.js")));
const fakeVersion=JSON.parse(await readFile(resolve(fakeDir,"package.json"),"utf8")).version;
Object.defineProperty(globalThis,"IDBKeyRange",{configurable:true,value:fake.IDBKeyRange});
const {IndexedDbStorage}=await source("packages/durable/src/storage/indexeddb.ts");
const {createStorageConformance}=await source("packages/durable/src/testing/storage-conformance.ts");
const {BACKGROUND_CONTEXT}=await source("packages/chord/src/context/index.ts");

const assertions={
 ok:assert.ok,strictEqual:assert.strictEqual,deepEqual:assert.deepStrictEqual,partialDeepEqual:assert.partialDeepStrictEqual,
 greaterThan(actual,expected){assert.ok(actual>expected,`${actual} should exceed ${expected}`);},
 async rejects(operation,includes){await assert.rejects(operation,error=>error instanceof Error && error.message.includes(includes));},
};
const arities={commit:1,mintId:0,conversation:1,scanConversations:3,entry:null,findLatestHeadMarker:2,scanEntries:3,task:1,scanTasks:3,submission:1,scanSubmissions:3,submissionByRequest:2,findDocument:2,document:2,scanDocuments:3,close:0};
const detach=value=>value===undefined?undefined:JSON.parse(JSON.stringify(value));
let current;
function instrument(storage){return new Proxy(storage,{get(target,name){
 const method=target[name];if(typeof method!=="function" || !(name in arities))return method;
 return async (...args)=>{
  const length=name==="entry"?args.length-1:arities[name];
  const record={method:name,args:detach(args.slice(0,length)),adopted:[]};current.push(record);
  try{
   const callArgs=[...args];
   if(name==="commit"){const original=args[1];callArgs[1]=seq=>{record.adopted.push(seq);original(seq);};}
   const result=await method.apply(target,callArgs);
   if(result===undefined)record.missing=true;else record.value=detach(result);
   return result;
  }catch(error){record.error={message:error.message,name:error.name};throw error;}
 };
}});}
const fixtures=fileURLToPath(new URL("../",import.meta.url));
async function emit(file,text){
 const target=resolve(fixtures,file);
 if(check)assert.equal(await readFile(target,"utf8"),text,`${file} differs from the reference`);
 else await writeFile(target,text);
}

// The conformance suite, as test/indexeddb-storage.test.ts registers it.
const variants={
 "indexeddb":async use=>{
  const indexedDB=new fake.IDBFactory();
  const storage=await IndexedDbStorage.open("conformance",{indexedDB});
  try{await use(storage);}finally{await storage.close(BACKGROUND_CONTEXT);}
 },
 "indexeddb-reopen":async use=>{
  const indexedDB=new fake.IDBFactory();const name="reopening-conformance";
  const created=await IndexedDbStorage.open(name,{indexedDB});await created.close(BACKGROUND_CONTEXT);
  let storage=await IndexedDbStorage.open(name,{indexedDB});let closed=false;
  const reopening=new Proxy({}, {get(_,method){return async (...args)=>{
   if(method==="close"){if(closed)return;closed=true;return storage.close(...args);}
   if(method!=="commit"||closed)return storage[method](...args);
   try{return await storage.commit(...args);}finally{await storage.close(BACKGROUND_CONTEXT);storage=await IndexedDbStorage.open(name,{indexedDB});}
  };}});
  try{await use(reopening);}finally{await reopening.close(BACKGROUND_CONTEXT);}
 },
};
for(const [variant,withStorage] of Object.entries(variants)){
 const records=[];
 const cases=createStorageConformance({assertions,withStorage:use=>withStorage(storage=>use(instrument(storage)))});
 for(const test of cases){current=[];await test.run();records.push({name:test.name,requests:current});}
 await emit(`storage-${variant}.json`,JSON.stringify({reference:pin,fakeIndexedDb:fakeVersion,cases:records},null,2)+"\n");
 await emit(`storage_${variant.replaceAll("-","_")}_cases.rs`,records.map((test,index)=>`storage_case!(${test.name.toLowerCase().replaceAll(/[^a-z0-9]+/g,"_").replaceAll(/^_|_$/g,"")}, ${index});`).join("\n")+"\n");
 console.log(`Reference ${variant} passed ${records.length} storage conformance cases`);
}

// Interchange: what the reference writes, record by record, and what it reads back.
const getAll=(store)=>new Promise((resolve,reject)=>{const request=store.getAll();request.onsuccess=()=>resolve(request.result);request.onerror=()=>reject(request.error);});
async function dump(indexedDB,name){
 const database=await new Promise((resolve,reject)=>{const request=indexedDB.open(name,1);request.onsuccess=()=>resolve(request.result);request.onerror=()=>reject(request.error);});
 try{
  const names=[...database.objectStoreNames];
  const transaction=database.transaction(names,"readonly");
  const stores={};
  for(const store of names){
   const objectStore=transaction.objectStore(store);
   stores[store]={keyPath:objectStore.keyPath,indexes:Object.fromEntries([...objectStore.indexNames].map(index=>[index,objectStore.index(index).keyPath])),rows:detach(await getAll(objectStore))};
  }
  return stores;
 }finally{database.close();}
}
const context=BACKGROUND_CONTEXT,adopt=()=>{};
const lone="\ud800";
async function writePhase(storage){
 await storage.commit([{type:"conversation",value:{id:1}}],adopt,context);
 const ids=[];for(let i=0;i<16;i++)ids.push(await storage.mintId());
 const [first,marker,task,background,owned,child,childEntry,submission,plainSubmission,notes,latest,session,taskDoc,retired,brief,copy]=ids;
 await storage.commit([
  {type:"entry",value:{id:first,conversationId:1,kind:"message",data:{text:`a${lone}b`,n:1.5}}},
  {type:"entry",value:{id:marker,conversationId:1,kind:"marker",head:first}},
  {type:"task",value:{id:task,conversationId:1,kind:`task.${lone}`,version:1,input:{value:3},state:{status:"pending",checkpoint:{phase:"ready"}},background:false,abortRequested:false}},
  {type:"task",value:{id:background,conversationId:1,kind:"test.background",version:2,input:null,state:{status:"running",checkpoint:{step:1}},background:true,abortRequested:true}},
  {type:"submission",value:{id:submission,conversationId:1,requestId:`request-${lone}`,type:"input",status:"queued"}},
  {type:"submission",value:{id:plainSubmission,conversationId:1,type:"input",status:"queued"}},
  {type:"document.create",record:{id:notes,kind:"notes",scope:{kind:"conversation",conversationId:1},history:"rewindable",fork:"asOf"},content:{kind:"base",version:1,value:{items:["a"],nested:{count:1}}}},
  {type:"document.create",record:{id:latest,kind:"latest",key:"k",scope:{kind:"conversation",conversationId:1},history:"latest",fork:"current"},content:{kind:"base",version:1,value:{v:1}}},
  {type:"document.create",record:{id:session,kind:"session.settings",key:lone,scope:{kind:"session"}},content:{kind:"base",version:1,value:{[lone]:lone}}},
  {type:"document.create",record:{id:taskDoc,kind:"task.state",scope:{kind:"task",taskId:task}},content:{kind:"base",version:1,value:{}}},
  {type:"document.create",record:{id:retired,kind:"retired",scope:{kind:"session"}},content:{kind:"base",version:1,value:{gone:true}}},
  {type:"document.create",record:{id:brief,kind:"brief",scope:{kind:"session"}},content:{kind:"base",version:1,value:{}}},
  {type:"document.retire",id:brief},
 ],adopt,context);
 await storage.commit([
  {type:"document.change",id:notes,content:{kind:"delta",version:1,ops:[["p",["items"],1,0,["b"]],["s",["nested","count"],2]]}},
  {type:"document.change",id:latest,content:{kind:"base",version:2,value:{v:2}}},
  {type:"document.retire",id:retired},
  {type:"conversation",value:{id:owned,owner:{conversationId:1,taskId:task}}},
  {type:"conversation",value:{id:child,parent:{conversationId:1,at:marker}}},
  {type:"task",value:{id:task,conversationId:1,kind:`task.${lone}`,version:1,input:{value:3},state:{status:"terminal",outcome:{status:"completed",result:{ok:true}}},background:false,abortRequested:false}},
 ],adopt,context);
 await storage.commit([
  {type:"entry",value:{id:childEntry,conversationId:child,kind:"child"}},
  {type:"document.change",id:notes,content:{kind:"delta",version:1,ops:[["s",["nested","count"],3]]}},
 ],adopt,context);
 await storage.commit([
  {type:"document.copy",record:{id:copy,kind:"notes",scope:{kind:"conversation",conversationId:child},history:"rewindable",fork:"asOf"},source:{id:notes,at:3}},
 ],adopt,context);
 return {first,marker,task,background,owned,child,childEntry,submission,notes,latest,session,taskDoc,retired,brief,copy};
}
async function readPhase(storage,ids){
 const {first,marker,task,owned,child,childEntry,notes,latest,session,taskDoc,retired,copy}=ids;
 await storage.conversation(owned,context);
 await storage.conversation(child,context);
 await storage.scanConversations({},10,undefined,context);
 await storage.scanConversations({ownerTaskId:task},10,undefined,context);
 await storage.scanConversations({ownerConversationId:1,order:"descending"},10,undefined,context);
 await storage.entry(first,context);
 await storage.entry(child,first,context);
 await storage.entry(child,childEntry,context);
 await storage.findLatestHeadMarker(child,undefined,context);
 await storage.findLatestHeadMarker(1,first,context);
 await storage.scanEntries({conversationId:child},10,undefined,context);
 const page=await storage.scanEntries({conversationId:child,order:"ascending"},1,undefined,context);
 await storage.scanEntries({conversationId:child,order:"ascending"},10,page.next,context);
 await storage.scanEntries({conversationId:1,minEntryId:marker},10,undefined,context);
 await storage.task(task,context);
 await storage.scanTasks({},10,undefined,context);
 await storage.scanTasks({status:"terminal"},10,undefined,context);
 await storage.scanTasks({kind:`task.${lone}`},10,undefined,context);
 await storage.scanTasks({abortRequested:true},10,undefined,context);
 await storage.scanTasks({background:false,order:"descending"},10,undefined,context);
 await storage.scanTasks({conversationId:1,background:true},10,undefined,context);
 await storage.scanSubmissions({},10,undefined,context);
 await storage.scanSubmissions({status:"queued",conversationId:1},1,undefined,context);
 await storage.submissionByRequest(1,`request-${lone}`,context);
 await storage.submissionByRequest(1,"missing",context);
 await storage.findDocument({kind:"notes",scope:{kind:"conversation",conversationId:1}},"current",context);
 await storage.findDocument({kind:"session.settings",key:lone,scope:{kind:"session"}},"current",context);
 await storage.findDocument({kind:"retired",scope:{kind:"session"}},"current",context);
 await storage.findDocument({kind:"retired",scope:{kind:"session"}},2,context);
 await storage.document(notes,"current",context);
 await storage.document(notes,2,context);
 await storage.document(notes,3,context);
 await storage.document(latest,"current",context);
 await storage.document(session,"current",context);
 await storage.document(taskDoc,"current",context);
 await storage.document(retired,"current",context);
 await storage.document(copy,"current",context);
 await storage.scanDocuments({scope:{kind:"session"},at:"current"},10,undefined,context);
 await storage.scanDocuments({scope:{kind:"session"},at:2},10,undefined,context);
 await storage.scanDocuments({scope:{kind:"conversation",conversationId:1},at:"current",kind:"notes"},10,undefined,context);
 await storage.scanDocuments({scope:{kind:"task",taskId:task},at:"current"},10,undefined,context);
 await storage.document(latest,2,context).catch(()=>{});
}
async function continuePhase(storage,ids){
 const next=await storage.mintId();
 await storage.commit([
  {type:"entry",value:{id:next,conversationId:ids.child,kind:"continued",head:ids.childEntry}},
  {type:"document.change",id:ids.notes,content:{kind:"delta",version:1,ops:[["s",["nested","count"],4]]}},
  {type:"document.retire",id:ids.latest},
 ],adopt,context);
 await storage.findLatestHeadMarker(ids.child,undefined,context);
 await storage.document(ids.notes,"current",context);
 await storage.findDocument({kind:"latest",key:"k",scope:{kind:"conversation",conversationId:1}},"current",context);
 await storage.commit([{type:"entry",value:{id:next,conversationId:1,kind:"duplicate"}}],adopt,context).catch(()=>{});
 await storage.mintId();
}
{
 const indexedDB=new fake.IDBFactory();const name="interchange";
 const phases={};
 let storage=instrument(await IndexedDbStorage.open(name,{indexedDB}));
 current=phases.write=[];const ids=await writePhase(storage);
 await storage.close(context);
 const stores=await dump(indexedDB,name);
 storage=instrument(await IndexedDbStorage.open(name,{indexedDB}));
 current=phases.read=[];await readPhase(storage,ids);
 current=phases.continue=[];await continuePhase(storage,ids);
 await storage.close(context);
 const finalStores=await dump(indexedDB,name);
 await emit("indexeddb-interchange.json",JSON.stringify({reference:pin,fakeIndexedDb:fakeVersion,phases,stores,finalStores},null,2)+"\n");
 console.log(`Reference interchange: ${phases.write.length}+${phases.read.length}+${phases.continue.length} requests over ${Object.keys(stores).length} object stores`);
}
