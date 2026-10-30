// Execute the original submission/inbox services without running generation.
// Native replay consumes only these independently generated observations.
import assert from "node:assert/strict";
import {execFileSync} from "node:child_process";
import {readFile,writeFile} from "node:fs/promises";
import {resolve} from "node:path";
import {pathToFileURL,fileURLToPath} from "node:url";
import {registerReferenceSource} from "./reference-source.mjs";
const reference=resolve(process.argv[2]??"../pi-mono"),pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);
registerReferenceSource(reference);
const load=path=>import(pathToFileURL(resolve(reference,path)));
const {MemoryStorage}=await load("packages/durable/src/storage/memory.ts");
const {SessionImpl}=await load("packages/durable/src/session/session.ts");
const {Submissions}=await load("packages/durable/src/harness/submissions.ts");
const {LiveDoc}=await load("packages/durable/src/harness/live.ts");
const {InboxDoc,prepareBoundary,applyBoundary,withdrawQueuedInputs}=await load("packages/durable/src/harness/inbox.ts");
const {BACKGROUND_CONTEXT:context}=await load("packages/chord/src/context/index.ts");
const input=(label,content,extra={})=>({op:"submit",label,draft:{type:"input",content,...extra}});
const write=(label,entry,extra={})=>({op:"submit",label,draft:{type:"write",entry,...extra}});
const boundary=at=>({op:"boundary",at});
const scenarios=[
 {name:"passive writes and per-conversation request identities",actions:[write("a",{kind:"note",data:null},{requestId:"r"}),write("again",{kind:"ignored"},{requestId:"r"}),input("conflict","x",{requestId:"r"}),{op:"conversation",label:"other"},{...write("other",{kind:"other"},{requestId:"r"}),conversation:"other"},{op:"get",label:"a"},{op:"abort",label:"a"}]},
 {name:"placed inputs deduplication and kind validation",actions:[input("a","hi",{requestId:"r",entry:{kind:"app.input",data:null}}),input("again","different",{requestId:"r",whenBusy:"reject"}),input("busy","busy",{whenBusy:"reject"}),input("invalid","x",{requestId:"r",entry:{kind:"pi.private"}}),input("empty","x",{entry:{kind:""}}),write("conflict",{kind:"note"},{requestId:"r"})]},
 {name:"resets precede users and stale heads cannot restore old ranges",actions:[input("active","a"),input("f1","f1"),write("reset",{kind:"pi.reset",head:"self"}),write("stale",{kind:"old",head:"first"}),write("display",{kind:"note",data:null}),input("s1","s1",{whenBusy:"steer"}),input("f2","f2"),boundary("postTools"),boundary("final")]},
 {name:"aborts remove queued inputs and writes with scoped results",actions:[input("active","a"),input("queued","q"),write("write",{kind:"queued",model:[]}),{op:"abort",label:"queued",conversationId:999},{op:"abort",label:"queued"},{op:"abort",label:"queued"},{op:"abort",label:"write"},{op:"abort",label:"active"},{op:"abort",id:999},{op:"get",id:999},{op:"status",id:999}]},
 {name:"withdrawal keeps writes and leaves placed input alone",actions:[input("active","a"),input("f","f"),input("s","s",{whenBusy:"steer"}),write("write",{kind:"queued",edits:[]}),{op:"withdraw",reason:"run_failed"},boundary("final")]},
 {name:"idle backlog selects the old followup before new admission",actions:[input("active","a"),input("old","old"),{op:"clearRun"},input("new","new"),input("rejected","later",{whenBusy:"reject"}),boundary("final")]},
 {name:"idle steering can be placed with the older followup",actions:[input("active","a"),input("old","old"),{op:"clearRun"},input("new","new",{whenBusy:"steer"})]},
 {name:"a display write queues behind a reset even when idle",actions:[input("active","a"),write("reset",{kind:"pi.reset",head:"self"}),input("old","old"),{op:"clearRun"},write("display",{kind:"note",data:{text:"after reset"}})]},
 {name:"idle head writes older than the active reset settle stale",actions:[input("active","a"),write("reset",{kind:"pi.reset",head:"self"}),boundary("final"),{op:"clearRun"},write("stale",{kind:"old",head:"first"})]},
 {name:"arbitrary UTF16 input request and data values survive admission",actions:[input("active","\ud800",{requestId:"\ud800",entry:{kind:"app.input",data:{"\udc00":null}}}),input("again","ignored",{requestId:"\ud800"}),input("queued",[{type:"text",text:"\udc00"}],{entry:{kind:"app.\ud800",data:null}}),boundary("final")]},
];
for(const steeringMode of ["all","one-at-a-time"])for(const followUpMode of ["all","one-at-a-time"]){
 scenarios.push({name:`queue modes ${steeringMode}/${followUpMode}`,actions:[{op:"modes",steeringMode,followUpMode},input("active","a"),input("s1","s1",{whenBusy:"steer"}),input("f1","f1"),input("s2","s2",{whenBusy:"steer"}),input("f2","f2"),write("w",{kind:"queued",model:[]}),boundary("postTools"),boundary("final")]});
}
const cases=[];
for(const retention of [600000,0])for(const scenario of scenarios){
 const storage=new MemoryStorage(),session=new SessionImpl(storage,()=>42,()=>retention);
 let modes={steeringMode:"one-at-a-time",followUpMode:"one-at-a-time"};
 const submissions=new Submissions(session,session.storage,()=>42,()=>modes,()=>{});
 const root=await session.commitWith(tx=>tx.createRootConversation(),context);
 const owners=new Map([["root",root.id]]),handles=new Map();
 const dump=async()=>({
  live:(await session.snapshot(LiveDoc,root.id,context))??null,
  inbox:(await session.snapshot(InboxDoc,root.id,context))??null,
  entries:(await storage.scanEntries({conversationId:root.id},1000,undefined,context)).items,
  tasks:(await storage.scanTasks({},1000,undefined,context)).items,
  submissions:(await storage.scanSubmissions({},1000,undefined,context)).items,
 });
 const observations=[];
 for(const action of scenario.actions){
  let result;
  const expected=action.label==="conflict"?"Error":action.label==="busy"||action.label==="rejected"?"ConversationBusy":action.label==="invalid"||action.label==="empty"?"TypeError":action.op==="status"&&action.id===999?"Error":undefined;
  try{
   const conversation=owners.get(action.conversation??"root"),id=action.id??handles.get(action.label)?.id;
   switch(action.op){
    case "submit":{
     const draft=structuredClone(action.draft);
     if(draft.entry?.head==="first"){const entries=(await storage.scanEntries({conversationId:conversation},1000,undefined,context)).items;draft.entry.head=entries.at(-1).id;}
     const handle=await submissions.submit(conversation,draft,context);handles.set(action.label,handle);result=await handle.status(context);break;
    }
    case "get":{const handle=await submissions.get(id,context);result=handle?.id;break;}
    case "status":result=await submissions.status(id,context);break;
    case "abort":result=await submissions.abort(id,context,action.conversationId);break;
    case "modes":modes={steeringMode:action.steeringMode,followUpMode:action.followUpMode};break;
    case "clearRun":await session.commitWith(async tx=>{delete (await tx.doc(LiveDoc,conversation)).run;},context);break;
    case "withdraw":await session.commitWith(tx=>withdrawQueuedInputs(tx,conversation,action.reason),context);break;
    case "boundary":result=await session.commitWith(async tx=>applyBoundary(tx,await prepareBoundary(tx,conversation,modes),action.at,42),context);break;
    case "conversation":{const record=await session.commitWith(tx=>tx.createConversation({ownership:{kind:"ownerless"}}),context);owners.set(action.label,record.id);result=record.id;break;}
    default:throw new Error(`Unknown action ${action.op}`);
   }
   assert.equal(expected,undefined,`Reference action was expected to fail: ${scenario.name}, ${JSON.stringify(action)}`);
   result={value:result??null};
  }catch(error){
   assert.equal(error.name,expected,`Unexpected reference failure in ${scenario.name}: ${error.stack}`);
   result={error:{name:error.name,message:error.message,...(error.conversationId===undefined?{}:{conversationId:error.conversationId})}};
  }
  observations.push({result,state:await dump()});
 }
 cases.push({name:scenario.name,retention,actions:scenario.actions,observations});await session.close(context);
}
const text=JSON.stringify({reference:pin,cases},null,2)+"\n";
const path=fileURLToPath(new URL("../../crates/pi-durable/tests/fixtures/submissions.json",import.meta.url));
if(process.argv.includes("--check"))assert.equal(await readFile(path,"utf8"),text,"Regenerate the reference submission fixtures");
else await writeFile(path,text);
console.log(`Reference submission/inbox traces: ${cases.length} cases, ${cases.reduce((sum,item)=>sum+item.actions.length,0)} observations; both retention modes`);
