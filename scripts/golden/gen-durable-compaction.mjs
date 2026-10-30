// Independently execute the source's range, sizing and transcript rules.
import assert from "node:assert/strict";
import {execFileSync} from "node:child_process";
import {readFile,writeFile} from "node:fs/promises";
import {resolve} from "node:path";
import {fileURLToPath,pathToFileURL} from "node:url";
import {registerReferenceSource} from "./reference-source.mjs";
const reference=resolve(process.argv[2]??"../pi-mono"),pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);registerReferenceSource(reference);
const load=path=>import(pathToFileURL(resolve(reference,path)));
const {selectCut,estimateContext,summarizedMessages,serializeConversation,compactionPolicy}=await load("packages/durable/src/harness/compaction.ts");
const {orderToolResults}=await load("packages/durable/src/harness/context.ts");
const {estimateMessageTokens}=await load("packages/ai/src/utils/estimate.ts");
const user=content=>({role:"user",content,timestamp:1});
const usage=totalTokens=>({input:3.5,output:1,cacheRead:2,cacheWrite:1.5,totalTokens,cost:{input:0,output:0,cacheRead:0,cacheWrite:0,total:0}});
const assistant=(content,totalTokens=0)=>({role:"assistant",content,provider:"faux",api:"faux",model:"fixture",usage:usage(totalTokens),stopReason:"stop",timestamp:2});
const text=text=>({type:"text",text}),call=(id,name="read",args={path:"a.ts"})=>({type:"toolCall",id,name,arguments:args});
const result=(id,content)=>({role:"toolResult",toolCallId:id,toolName:"read",content,timestamp:3});
const system={role:"system",content:"base",sections:JSON.parse('{"10":"ten","2":"two","z":"last","removed":null}'),toolsAdded:[{name:"read",description:"read a file",parameters:JSON.parse('{"type":"object","properties":{"\\ud800":{"type":"string"}}}')}],toolsRemoved:[{name:"old"}],timestamp:0};
const sources=[
 {name:"empty",groups:[]},
 {name:"ordinary",groups:[[user("x".repeat(350))],[assistant([text("a".repeat(350))])],[user("b".repeat(700))],[assistant([text("c".repeat(100))])]]},
 {name:"delayed receipts and steering",groups:[[user("start")],[assistant([text("call"),call("c1")])],[user("steer".repeat(100))],[result("c1",[text("r".repeat(12000))])],[assistant([text("next")])]]},
 {name:"multi-call receipts before next assistant",groups:[[user("start")],[assistant([call("c1"),call("c2")])],[user("steer")],[result("c1",[text("one")])],[result("c2",[text("two")])],[assistant([text("last")])]]},
 {name:"user with assistant in same contribution",groups:[[user("old")],[assistant([call("c")])],[user("steer"),assistant([text("new response")])],[result("c",[text("late")])]]},
 {name:"edited display and system contributions",groups:[[user("old".repeat(250))],[],[system],[],[assistant([text("tail")],350)],[user("last")]]},
 {name:"marker only is not summarized",head:true,groups:[[user("summary")],[user("tail".repeat(1000))]]},
 {name:"measured usage after marker",head:true,groups:[[user("earlier summary")],[user("start")],[assistant([text("measured")],123.5)],[user("tail")]]},
 {name:"usage before marker does not apply",head:true,groups:[[assistant([text("old")],999)],[user("start")],[user("next".repeat(400))]]},
 {name:"UTF16 text images calls and split truncation",groups:[[system],[user([text("\ud800"),{type:"image",data:"ignored",mimeType:"image/png"}])],[assistant([{type:"thinking",thinking:"hidden"},text("\udfff"),call("c","\ud800",JSON.parse('{"10":1,"2":2,"__proto__":"own","\\ud800":"\\udfff"}'))])],[result("c",[text("x".repeat(1999)+"\ud834\udd1e"+"z".repeat(7))])],[user("tail")]]},
];
const cases=[];
for(const source of sources){
 const contributions=source.groups,entries=contributions.map((model,index)=>({id:index+1,conversationId:1,kind:index===0&&source.head?"pi.compaction":"fixture",model}));
 const view={...(source.head?{head:entries[0]}:{}),entries,contributions,messages:orderToolResults(contributions.flat())};
 const aliases=view.messages.map(message=>{for(let i=0;i<contributions.length;i++){const j=contributions[i].indexOf(message);if(j>=0)return[i,j];}return null;});
 const extra=[system,user("extra")];
 const cuts=[0,1,50,150,1000,2000,20000].map(budget=>({budget,cut:selectCut(view,budget)??null}));
 cases.push({name:source.name,view,aliases,extra,cuts,tokens:estimateContext(view,extra),messageTokens:contributions.map(messages=>messages.map(estimateMessageTokens)),serialized:serializeConversation(view.messages),prefixes:Array.from({length:contributions.length+1},(_,cut)=>summarizedMessages(view,cut))});
}
const policies=[];
for(const model of [{provider:"faux",modelId:"fixture"},{provider:"\ud800",modelId:"\udfff"},{provider:"other",modelId:"different"}]){
 const compaction={enabled:true,reserveTokens:16384,keepRecentTokens:20000,backgroundTokens:32768,modelOverrides:{"faux/fixture":{enabled:false,reserveTokens:100.5,keepRecentTokens:null},"\ud800/\udfff":{backgroundTokens:0,keepRecentTokens:12.5}}};
 policies.push({model,compaction,expected:compactionPolicy({compaction},model)});
}
const output=JSON.stringify({reference:pin,cases,policies},null,2)+"\n";
const path=fileURLToPath(new URL("../../crates/pi-durable/tests/fixtures/compaction.json",import.meta.url));
if(process.argv.includes("--check"))assert.equal(await readFile(path,"utf8"),output,"Regenerate reference compaction fixture");else await writeFile(path,output);
console.log(`Reference compaction: ${cases.length} views, ${cases.reduce((sum,value)=>sum+value.cuts.length,0)} cuts, ${policies.length} policies`);
