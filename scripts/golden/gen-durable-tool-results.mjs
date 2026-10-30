// Direct observations of the pinned result writer, independent of native code.
import assert from "node:assert/strict";
import {execFileSync} from "node:child_process";
import {readFile,writeFile} from "node:fs/promises";
import {resolve} from "node:path";
import {pathToFileURL,fileURLToPath} from "node:url";
import {registerReferenceSource} from "./reference-source.mjs";
const reference=resolve(process.argv[2]??"../pi-mono"),pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);registerReferenceSource(reference);
const load=path=>import(pathToFileURL(resolve(reference,path)));
const {MemoryStorage}=await load("packages/durable/src/storage/memory.ts");
const {SessionImpl}=await load("packages/durable/src/session/session.ts");
const {appendToolResult}=await load("packages/durable/src/harness/tool.ts");
const {UsageDoc}=await load("packages/durable/src/harness/usage.ts");
const {BACKGROUND_CONTEXT:context}=await load("packages/chord/src/context/index.ts");
const text=text=>({type:"text",text}),call={type:"toolCall",id:"call-1",name:"read",arguments:{path:"a.ts"}};
const usage={input:1.5,output:2,cacheRead:3,cacheWrite:4,totalTokens:10.5,cost:{input:0.01,output:0.02,cacheRead:0.03,cacheWrite:0.04,total:0.1}};
const samples=[
 {name:"empty",result:{}},
 {name:"null details and structured channels do not enter model content",result:{details:null,structuredOutput:null,control:{terminate:false,handoff:"ignored",addTools:["ignored"]}}},
 {name:"explicit output and diagnostics",result:{output:[text("one"),text("two")],diagnostics:[{severity:"info",message:"hint"},{severity:"warn",message:"trimmed",code:"truncated"},{severity:"error",message:"failed",code:"error"}]}},
 {name:"images and text",result:{output:[{type:"image",data:"aGVsbG8=",mimeType:"image/png"},text("visible")],isError:false,details:{text:"ui only"}}},
 {name:"UTF16 name identities and diagnostics",call:{...call,id:"\ud800",name:"\udfff"},result:{output:[text("\ud800")],isError:true,details:JSON.parse('{"\\ud800":"\\udfff"}'),diagnostics:[{severity:"error",message:"\udfff",code:"\ud800"}]}},
 {name:"usage recorded once with receipt",result:{output:[text("spent")],usage}},
 {name:"fractional usage key and explicit zero duration",call:{...call,name:"__proto__"},result:{isError:true,usage},durationMs:0},
 {name:"fractional duration and timestamp",result:{output:[],diagnostics:[]},durationMs:0.5,timestamp:42.25},
];
const cases=[];
for(const retention of [600000,0])for(const sample of samples){
 const storage=new MemoryStorage(),session=new SessionImpl(storage,()=>42,()=>retention),root=await session.commitWith(tx=>tx.createRootConversation(),context);
 const callValue=sample.call??call,meta={timestamp:sample.timestamp??42,...(sample.durationMs===undefined?{}:{durationMs:sample.durationMs})};
 const entry=await session.commitWith(tx=>appendToolResult(tx,root.id,callValue,sample.result,meta),context);
 const state=(await session.snapshot(UsageDoc,root.id,context))??null;
 cases.push({name:sample.name,retention,call:callValue,result:sample.result,meta,entry,usage:state});await session.close(context);
}
const {assignJson}=await load("packages/durable/src/harness/json.ts");
const {track}=await load("packages/chord/src/delta/index.ts");
const assignments=[];
for(const [name,current,value] of [
 ["new field",{},null],
 ["record delete before numeric key order",{value:{old:1,z:2}},{"10":"ten","2":"two",z:3}],
 ["append existing leaf",{value:{text:"hello",other:1}},{text:"hello world",other:1}],
 ["grow array without rewriting existing objects",{value:[{text:"a"},2]},[{text:"ab"},3,{text:"new"}]],
 ["shrink array replaces field",{value:[1,2,3]},[1]],
 ["identical records preserve noops",{value:{a:[1,{text:"x"}]}},{a:[1,{text:"x"}]}],
 ["UTF16 keys and leaves",{value:JSON.parse('{"\\ud800":"\\udfff"}')},JSON.parse('{"\\ud800":"\\udfffmore"}')],
 ["nested record to array",{value:{a:{b:1}}},{a:[1,2]}],
 ["array to scalar",{value:[1]},false],
 ["null to record",{value:null},{ok:true}],
]){
 const tracker=track(current),change=tracker.beginChange();assignJson(change.state,"value",value);const prepared=change.prepare();
 assignments.push({name,current,value,after:prepared.value,ops:prepared.ops});change.abort();
}
const output=JSON.stringify({reference:pin,cases,assignments},null,2)+"\n",path=fileURLToPath(new URL("../../crates/pi-durable/tests/fixtures/tool-results.json",import.meta.url));
if(process.argv.includes("--check"))assert.equal(await readFile(path,"utf8"),output,"Regenerate reference tool result fixtures");else await writeFile(path,output);
console.log(`Reference tool results: ${cases.length} result/usage traces and ${assignments.length} leaf assignments`);
