// Independently executed Session scenarios over the pinned, unmodified reference.
import assert from "node:assert/strict";
import {execFileSync} from "node:child_process";
import {readFile,writeFile} from "node:fs/promises";
import {resolve} from "node:path";
import {pathToFileURL,fileURLToPath} from "node:url";
const reference=resolve(process.argv[2]??"../pi-mono");
const pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);
const load=path=>import(pathToFileURL(resolve(reference,path)));
const {MemoryStorage}=await load("packages/durable/src/storage/memory.ts");
const {createSession}=await load("packages/durable/src/session/session.ts");
const {defineDoc,defineDocFamily}=await load("packages/durable/src/documents.ts");
const {defineTask}=await load("packages/durable/src/tasks.ts");
const {BACKGROUND_CONTEXT:context}=await load("packages/chord/src/context/index.ts");
const definitions={
 counter:{kind:"counter",version:1,scope:"session"},
 v2:{kind:"counter",version:2,scope:"session",migrate:true},
 checkpoint:{kind:"checkpoint",version:1,scope:"session",checkpointAfter:2},
 family:{kind:"member",version:1,scope:"session",family:true},
 historical:{kind:"historical",version:1,scope:"conversation",history:"rewindable",fork:"asOf"},
 current:{kind:"current",version:1,scope:"conversation",history:"latest",fork:"current"},
 initial:{kind:"initial",version:1,scope:"conversation",history:"latest",fork:"initial"},
 task:{kind:"scratch",version:1,scope:"task"},
};
const c=(...steps)=>({op:"commit",steps});
const doc=(token,count,extra={})=>({op:"doc",token,...(count===undefined?{}:{count}),...extra});
const snapshot=(token,extra={})=>({op:"snapshot",token,...extra});
const scenarios=[
 {name:"creation noop edits rollback and retirement",steps:[snapshot("counter"),c(doc("counter",1)),c(doc("counter")),snapshot("counter"),{...c(doc("counter",99)),fail:true},snapshot("counter"),c({op:"retire",token:"counter"},doc("counter",7)),snapshot("counter")]},
 {name:"migration remains in memory until commit",steps:[c(doc("counter",4)),snapshot("v2"),snapshot("counter"),c(doc("v2")),snapshot("v2"),{...snapshot("counter"),error:true}]},
 {name:"checkpoint delta accounting",steps:[c(doc("checkpoint",0)),c(doc("checkpoint",1)),c(doc("checkpoint",2)),c(doc("checkpoint",3)),c(doc("checkpoint",4)),snapshot("checkpoint")]},
 {name:"family incarnation ownership and seeds",steps:[c(doc("family",undefined,{key:"a",seed:{count:3}}),doc("family",5,{key:"a",seed:{count:9}}),doc("family",undefined,{key:"b",seed:{count:10}})),snapshot("family",{key:"a"}),snapshot("family",{key:"b"}),c({op:"retire",token:"family",key:"a"},doc("family",undefined,{key:"a",seed:{count:11}})),snapshot("family",{key:"a"})]},
 {name:"fork policies and typed copy acquisition",steps:[c({op:"conversation",save:"parent"}),c(doc("historical",1,{owner:"parent"}),doc("current",2,{owner:"parent"}),doc("initial",3,{owner:"parent"}),{op:"entry",owner:"parent",save:"cutoff"}),c(doc("historical",4,{owner:"parent"}),doc("current",5,{owner:"parent"})),c({op:"fork",parent:"parent",at:"cutoff",save:"child"}),snapshot("historical",{owner:"child"}),snapshot("current",{owner:"child"}),snapshot("initial",{owner:"child"}),c(doc("historical",9,{owner:"child"}),doc("initial",undefined,{owner:"child"})),snapshot("historical",{owner:"parent"}),snapshot("historical",{owner:"child"})]},
 {name:"fork rejects source mutations atomically",steps:[c({op:"conversation",save:"parent"}),c(doc("historical",1,{owner:"parent"}),{op:"entry",owner:"parent",save:"cutoff"}),{...c({op:"fork",parent:"parent",at:"cutoff",save:"child"},doc("historical",2,{owner:"parent"})),error:true},snapshot("historical",{owner:"parent"})]},
 {name:"task lifecycle and submission settlement",steps:[c({op:"conversation",save:"parent"}),c({op:"task",owner:"parent",save:"work"}),c(doc("task",6,{owner:"work"}),{op:"submission",owner:"parent",save:"request"},{op:"entry",owner:"parent",save:"answer"},{op:"place",submission:"request",entry:"answer"},{op:"settle",submission:"request",answer:"answer"},{op:"terminal",task:"work"}),snapshot("task",{owner:"work"})]},
 {name:"validation precedes checkpoint callbacks",steps:[c({op:"conversation",save:"parent"}),c({op:"task",owner:"parent",save:"work"}),{...c(doc("checkpoint",9),{op:"conversation",task:"work",save:"owned"},{op:"terminal",task:"work"}),error:true},snapshot("checkpoint")]},
];
const copy=value=>value===undefined?null:JSON.parse(JSON.stringify(value));
const cases=[];
for(const retention of [0,Infinity]) for(const scenario of scenarios){
 const writes=[],publications=[],env={};
 class Recorded extends MemoryStorage {async commit(batch,adopt,ctx){writes.push(copy(batch));return super.commit(batch,adopt,ctx);}}
 const storage=new Recorded();const session=createSession(storage,{now:()=>1234,documentRetentionMs:retention,setTimer:()=>undefined});
 session.subscribeCommits(publication=>publications.push(copy(publication)));
 const tokens=Object.fromEntries(Object.entries(definitions).map(([key,spec])=>{
  const definition={...spec,initial:spec.family?seed=>seed:()=>({count:0}),checkpointWhen:(_value,_ops,info)=>spec.checkpointAfter!==undefined&&info.deltasSinceBase>=spec.checkpointAfter,
   ...(spec.migrate?{migrate:value=>({...value,label:"migrated"})}:{})};
  return [key,(spec.family?defineDocFamily:defineDoc)(definition)];
 }));
 const task=defineTask({name:"test",version:1,initial:()=>({phase:"start"}),phases:{start:async()=>{}},abort:async()=>{}});
 const args=step=>[tokens[step.token],...(step.owner?[env[step.owner]]:[]),...(step.key!==undefined?[step.key]:[])];
 const records=[];
 for(const step of scenario.steps){
  try {
   let result;
   if(step.op==="snapshot") result=await session.snapshot(...args(step),context);
   else {
    result=await session.commit(async tx=>{
     const values=[];
     for(const action of step.steps){let value;
      switch(action.op){
       case "conversation":value=await tx.createConversation({ownership:action.task?{kind:"task",taskId:env[action.task]}:{kind:"ownerless"}});break;
       case "fork":value=await tx.forkConversation(env[action.parent],env[action.at],{ownership:{kind:"ownerless"}});break;
       case "entry":value=await tx.appendEntry(env[action.owner],{kind:"note",head:"self"});break;
       case "doc":{const draft=await tx.doc(...args(action),...(action.seed?[action.seed]:[]));if(action.count!==undefined)draft.count=action.count;value=copy(draft);break;}
       case "retire":await tx.retireDoc(...args(action));value=null;break;
       case "task":value=await tx.createTask(task,null,{ownership:{kind:"conversation"},conversationId:env[action.owner],trace:"root"});break;
       case "submission":value=await tx.createSubmission({conversationId:env[action.owner],type:"input",status:"queued"});break;
       case "place":tx.placeSubmission(env[action.submission],env[action.entry]);value=null;break;
       case "settle":tx.settleSubmission(env[action.submission],{status:"done",answer:env[action.answer]});value=null;break;
       case "terminal":{const record=tx.stagedTasks().find(t=>t.id===env[action.task])??await storage.task(env[action.task],context);tx.setTask({...record,state:{status:"terminal",outcome:{status:"completed",result:null}}});value=null;break;}
       default:throw new Error(`Unknown ${action.op}`);
      }
      if(action.save)env[action.save]=typeof value==="number"?value:value.id;
      values.push(copy(value));
     }
     if(step.fail)throw new Error("callback failed");return values;
    },context);
   }
   assert.ok(!step.error&&!step.fail,"Expected scenario to reject");records.push({value:copy(result)});
  }catch(error){if(!step.error&&!step.fail)throw error;records.push({error:{name:error.name,message:error.message}});}
 }
 await session.close(context);
 cases.push({name:`${scenario.name} (retention ${retention===0?"zero":"infinite"})`,retention:retention===0?0:null,steps:scenario.steps,records,writes,publications});
}
const output=JSON.stringify({reference:pin,definitions,cases},null,2)+"\n";
const target=fileURLToPath(new URL("../../crates/pi-durable/tests/fixtures/session.json",import.meta.url));
if(process.argv.includes("--check"))assert.equal(await readFile(target,"utf8"),output,"Session traces differ from reference");else await writeFile(target,output);
console.log(`Reference passed ${cases.length} Session traces and ${cases.reduce((n,c)=>n+c.steps.length,0)} operations`);
