import {describeTelemetry} from "./durable-telemetry-adapter.mjs";
import {installSessionBinding,getDefaultDocumentRetention} from "./durable-session-adapter.mjs";
import "./durable-transaction-probes.mjs";
// Test transport only. The native process owns task state, invocation lifetime,
// reservations and the Session line. These callbacks run the original JS tasks.
import {nativeMemoryHandle} from "./durable-memory-adapter.mjs";
import {nativeSqliteHandle} from "./durable-task-sqlite-adapter.mjs";
import {environmentContext} from "./durable-env-signals.mjs";
import {writeFileSync} from "node:fs";
import {mergeClientIdentity,withClientIdentity} from "@earendil-works/pi-ai/utils/identity";
import {ConversationBusy,ReadOnlySession,ReadAfterWrite,DEFAULT_HARNESS_IDENTITY} from "@earendil-works/pi-durable";
import {validateToolArguments} from "@earendil-works/pi-ai/utils/validation";
import {BACKGROUND_CONTEXT,withAbortSignal,withoutAbortSignal} from "@earendil-works/chord/context";
import {copyJson} from "@earendil-works/chord";
import {track} from "@earendil-works/chord/delta";
import {nativeTransport,failure as error} from "./durable-native-transport.mjs";
import {handle,describeTask,describeDoc,objectFor,valueHandle,temporaryHandle} from "./durable-native-handles.mjs";
import {settingsRequest,hydrateSettings} from "./durable-settings-wire.mjs";
import {nativeRegistryId,lookupNativeSnapshot,snapshot,hydrateAgent} from "./durable-registry-adapter.mjs";
nativeTransport.register("docInitial",({id,seed})=>copyJson(objectFor(id).definition.initial(seed)));
nativeTransport.register("docMigrate",([id,value,version])=>copyJson(objectFor(id).definition.migrate(value,version)));
nativeTransport.register("docCheckpoint",({id,value,ops,info})=>objectFor(id).definition.checkpointWhen(graphValue(value),graphValue(ops),info));
nativeTransport.error("conversationBusy",value=>new ConversationBusy(value.conversationId));
nativeTransport.error("readOnly",value=>new ReadOnlySession(value.operation));
nativeTransport.error("readAfterWrite",value=>new ReadAfterWrite(value.method));
let storageNext=1,callbackNext=1;
const storages=new Map(),callbacks=new Map(),runtimes=new Map(),harnesses=new Map(),graphRevisions=new Map();
const drafts=new Map(),viewBindings=new WeakMap();
const crashOwners=new WeakMap();
function trackCrashFixture(storage,owner){
 if(typeof storage.crash!=="function")return;
 let owners=crashOwners.get(storage);
 if(owners===undefined){
  owners=new Set();crashOwners.set(storage,owners);const crash=storage.crash;
  Object.defineProperty(storage,"crash",{configurable:true,writable:true,value:function(...args){const result=Reflect.apply(crash,this,args);for(const owner of owners)nativeTransport.crashActor(owner);owners.clear();return result;}});
 }
 owners.add(owner);
}

// Table identities cross this JSON-only test transport as record/revision identities.
// This does not claim pointer or allocation parity for Rust's owned record types.
const tableRecords=new Map(),tableWrites=new Map();
function tableRecord(type,value){if(value===undefined)return value;const key=type+":"+value.id+":"+JSON.stringify(value);if(!tableRecords.has(key))tableRecords.set(key,value);return tableRecords.get(key);}
function tableWrite(write){if(!["conversation","entry","task","submission"].includes(write.type))return write;const value=tableRecord(write.type,write.value),key=write.type+":"+value.id+":"+JSON.stringify(value);if(!tableWrites.has(key))tableWrites.set(key,{...write,value});return tableWrites.get(key);}
const sharedNodes=new Map();
function sharedValue(node){
 if("scalar" in node)return node.scalar;
 if(sharedNodes.has(node.id))return sharedNodes.get(node.id);
 const value="array" in node?node.array.map(sharedValue):Object.fromEntries(node.object.map(([key,child])=>[key,sharedValue(child)]));
 sharedNodes.set(node.id,value);return value;
}
function graphValue(snapshot){if("shared" in snapshot)return sharedValue(snapshot.shared);if(!graphRevisions.has(snapshot.revision))graphRevisions.set(snapshot.revision,snapshot.value);return graphRevisions.get(snapshot.revision);}
function watchValue(snapshot){return graphValue(snapshot);}
nativeTransport.register("graphFrame",(args,context)=>objectFor(args.callback)(watchValue(args.snapshot),graphValue(args.ops),withoutAbortSignal(context??BACKGROUND_CONTEXT)));
nativeTransport.afterCallback("graphFrame",frame=>nativeTransport.sync("graph.turn",{watch:frame.args.watch,serial:frame.args.serial}));
nativeTransport.afterCallback("phase",frame=>nativeTransport.sync("phase.turn",{runtime:frame.args[3],serial:frame.args[4],harness:frame.args[5]}));
nativeTransport.afterCallback("change",frame=>nativeTransport.sync("change.turn",{request:frame.args.request}));
nativeTransport.afterCallback("onWake",frame=>nativeTransport.sync("host.turn",{harness:frame.args.harness}));
function graphWatch(descriptor,context,source){
 if(descriptor===undefined)return undefined;
 const watch=descriptor.watch,closed=call("graph.closed",{watch}).then(end=>end.error?{...end,error:error(end.error)}:end);closed.catch(()=>{});
 let stopped,listener;const held={source},signal=context?.abortSignal,cancel=()=>nativeTransport.sync("graph.cancel",{watch});
 signal?.addEventListener("abort",cancel,{once:true});
 const release=()=>{held.source=undefined;listener?.release();signal?.removeEventListener("abort",cancel);};
 closed.then(release,release);
 if(signal?.aborted)cancel();
 return {get value(){return watchValue(nativeTransport.sync("graph.value",{watch}));},closed,start:callback=>{const next=temporaryHandle(callback);try{nativeTransport.sync("graph.start",{watch,callback:next.id});listener=next;}catch(error){next.release();throw error;}},stop:()=>{if(stopped===undefined){nativeTransport.sync("graph.stop",{watch});stopped=closed;}return stopped;},cancel};
}
function docArguments(token,args){const definition=describeDoc(token);let index=0;const scope=definition.scope==="session"?{kind:"session"}:definition.scope==="task"?{kind:"task",taskId:args[index++]}:{kind:"conversation",conversationId:args[index++]};const address={kind:definition.kind,scope};if(definition.family)address.key=args[index++];return {definition,address,index};}
function documents(owner,source){return {
 snapshot:(token,...args)=>{const {index,...document}=docArguments(token,args);return call("doc.snapshot",{...owner,...document},args[index]).then(value=>value===undefined?undefined:graphValue(value));},
 snapshotAsOf:(token,...args)=>{const {index,...document}=docArguments(token,args);return call("doc.snapshotAsOf",{...owner,...document,at:args[index]},args[index+1]).then(value=>value===undefined?undefined:graphValue(value));},
 watchDoc:(token,...args)=>{const {index,...document}=docArguments(token,args);return call("doc.watch",{...owner,...document},args[index]).then(value=>graphWatch(value,args[index],source));},
};}
function inspect(value){for(const entry of value.tasks)if(entry.state.error)entry.state.error=error(entry.state.error);return value;}
const stats={phases:0,commits:0,storageCalls:0};
const call=(method,args={},context)=>nativeTransport.request(method,args,context);
const send=frame=>nativeTransport.notify(frame);
nativeTransport.event("signal",frame=>runtime(frame.runtime).controller.abort(new Error(frame.message)));
nativeTransport.event("report",frame=>{try{Promise.resolve(harnesses.get(frame.harness)?.onReport?.(error(frame.error),frame.info)).catch(()=>{});}catch{}});
nativeTransport.register("timerFire",id=>objectFor(id)());
nativeTransport.register("setTimer",({harness,timer,delay,background})=>{
 const arm=harnesses.get(harness)?.options.setTimer??sessions.get(harness)?.setTimer;
 const disarm=arm(()=>nativeTransport.sync("hostTimerFire",{timer}),delay,{background});
 return disarm===undefined?null:handle(disarm);
});
nativeTransport.register("disarmTimer",id=>{objectFor(id)();return null;});
nativeTransport.register("tool.prepare",({tool,arguments:args})=>(objectFor(tool).prepareArguments===undefined?args:objectFor(tool).prepareArguments(args)));
nativeTransport.register("tool.validate",({tool,call,arguments:args})=>validateToolArguments(objectFor(tool),{...call,arguments:args}));
nativeTransport.register("tool.execute",async(args,context)=>{const api=toolApi(args);try{return packToolResult(await objectFor(args.tool).execute(args.arguments,api,withAbortSignal(runtime(args.runtime).signal,context??BACKGROUND_CONTEXT)));}finally{toolApiStates.get(api).ended=true;}});
nativeTransport.register("onWake",({harness,wake})=>{harnesses.get(harness).options.onWake(wake);return null;});
nativeTransport.register("documentRetention",id=>{const input=harnesses.get(id)?.options.settings,settings=typeof input==="function"?input():input,value=settings?.documentRetentionMs;return typeof value!=="number"?null:Number.isFinite(value)?value:String(value);});
nativeTransport.register("conversationCreated",args=>captureChange(args.tx,tx=>harnesses.get(args.harness).options.conversationCreated(tx,args.record)));
nativeTransport.register("now",id=>harnesses.get(id)?.options.now?.()??Date.now());
nativeTransport.register("settings",id=>{const input=harnesses.get(id)?.options.settings;return settingsRequest(typeof input==="function"?input():input);});
nativeTransport.register("readerSnapshot",id=>{const value=objectFor(id).snapshot();return lookupNativeSnapshot(value)??{tasks:value.tasks().map(describeTask)};});
nativeTransport.register("readerSubscribe",([id,listener])=>handle(objectFor(id).subscribe(()=>nativeTransport.sync("registry.notifyReader",{listener}))));
nativeTransport.register("readerUnsubscribe",id=>objectFor(id)());
nativeTransport.register("hookMember",({handlers,name})=>{const owner=objectFor(handlers),member=owner[name];return typeof member==="function"?handle(member.bind(owner)):null;});
nativeTransport.register("hookInvoke",({callback,handler})=>objectFor(callback)(objectFor(handler)));
for(const method of ["phase","change","storage"])nativeTransport.register(method,(args,context,frame)=>invoke(method,args,frame.abortable?(context??BACKGROUND_CONTEXT):withoutAbortSignal(context??BACKGROUND_CONTEXT)));
function change(method,args,callback,context){
 const id=callbackNext++;callbacks.set(id,callback);const request=nativeTransport.nextId;
 const operation=call(method,{...args,callback:id},context).finally(()=>callbacks.delete(id));operation.catch(()=>{});
 // Bring immediate native work to its first host promise before a JS timer.
 // Pumped asynchronous callbacks resume as microtasks after this call returns.
 // A synchronous host callback must return before its queued writes can run.
 if(!nativeTransport.inSynchronousCallback)nativeTransport.sync("change.turn",{request});
 return operation;
}
function configuration(change){
 const result={...change};
 for(const field of ["extensions","tools","modelTools"]){
  const value=change[field];const names=items=>items.map(item=>typeof item==="object"&&item!==null?{name:item.name}:item);
  if(Array.isArray(value))result[field]=names(value);
  else if(value&&typeof value==="object")result[field]=Object.fromEntries(Object.entries(value).map(([key,items])=>[key,Array.isArray(items)?names(items):items]));
 }
 return result;
}
function transaction(id){const tx={
 conversation:conversation=>call("tx.conversation",{tx:id,conversation}),
 scanConversations:(query,limit,cursor)=>call("scanConversations",{tx:id,query,limit,cursor}),
 task:task=>call("tx.task",{tx:id,task}),setTask:record=>nativeTransport.sync("tx.setTask",{tx:id,record}),
 doc:async(token,...args)=>{const {index,...document}=docArguments(token,args);const acquired=await call("doc.acquire",{tx:id,...document,seed:token.definition.family?args[index]:undefined});let local=drafts.get(id);if(!local){local=new Map();drafts.set(id,local);}let held=local.get(acquired.draft);if(!held){const tracker=track(acquired.value);held={tracker,change:tracker.beginChange()};local.set(acquired.draft,held);}return held.change.state;},
 retireDoc:(token,...args)=>{const {index,...document}=docArguments(token,args);return call("doc.retire",{tx:id,...document});},
 latestHeadMarker:conversation=>call("tx.latestHeadMarker",{tx:id,conversation}),
 createConversation:options=>call("tx.createConversation",{tx:id,options}).then(value=>tableRecord("conversation",value)),
 forkConversation:(conversation,at,options)=>call("tx.forkConversation",{tx:id,conversation,at,options}),
 createTask:(task,input,options)=>call("createTask",{tx:id,definition:describeTask(task),input,options}),
 appendEntry:async(first,second,third)=>{const entry=copyJson(typeof first==="number"?second:{...third,kind:first.kind},{omitUndefinedProperties:true});return tableRecord("entry",await call("appendEntry",{tx:id,conversation:typeof first==="number"?first:second,entry}));},
 entry:(first,second)=>typeof first==="number"?call("entry",{tx:id,entry:first}):call("entry",{tx:id,kind:first.kind,entry:second}),
 createSubmission:record=>call("createSubmission",{tx:id,record}),
 placeSubmission:(submission,entry)=>call("placeSubmission",{tx:id,submission,entry}),
 settleSubmission:(submission,settlement)=>call("settleSubmission",{tx:id,submission,settlement}),
 scanEntries:(query,limit,cursor)=>call("scanEntries",{tx:id,query,limit,cursor}),
 scanTasks:(query,limit,cursor)=>call("scanTasks",{tx:id,query,limit,cursor}),
};
 // Native owns and drains every admitted operation. Observe its JS wrapper too,
 // including deliberately unawaited calls whose rejection is inspected later.
 for(const name of Object.keys(tx))if(name!=="setTask"){
  const invoke=tx[name];tx[name]=(...args)=>{let result;try{result=Promise.resolve(invoke(...args));}catch(error){result=Promise.reject(error);}result.catch(()=>{});return result;};
 }
 return tx;
}
function boundSubmission({id,handle}){return {id,status:context=>call("bound.submission.status",{handle},context),wait:context=>call("bound.submission.wait",{handle},context),abort:context=>call("bound.submission.abort",{handle},context)};}
function boundConversation({id,handle}){return {id,submit:(draft,context)=>call("bound.submit",{handle,draft},context).then(boundSubmission),abort:(context,options)=>call("bound.abort",{handle,background:options?.background},context),waitForIdle:context=>call("bound.waitForIdle",{handle},context)};}
export function runtime(id){
 let held=runtimes.get(id);if(held)return held;
 const controller=new AbortController();
 held={controller,signal:controller.signal,get settings(){return hydrateSettings(nativeTransport.sync("runtimeSettings",{runtime:id}));},get models(){return harnesses.get(held.harness)?.currentModels();},env:context=>call("runtimeEnv",{runtime:id},context).then(id=>objectFor(id)),setTimer:(callback,delay)=>{const timer=nativeTransport.sync("runtimeTimer",{runtime:id,callback:handle(callback),delay});return()=>nativeTransport.sync("runtimeDisarmTimer",{timer});},entry:(token,entry,context)=>call("runtimeEntry",{runtime:id,kind:token.kind,entry},context),agent:context=>call("runtimeAgent",{runtime:id},context).then(hydrateAgent),hooks:{each:(name,invoke)=>call("runtimeHooks",{runtime:id,name,callback:handle(invoke)})},get registry(){return snapshot(nativeTransport.sync("runtimeRegistry",{runtime:id}));},commit:(callback,context)=>change("commit",{runtime:id},callback,context),
  memo:(name,...args)=>args.length===1?call("memo",{runtime:id,name},args[0]):call("memo",{runtime:id,name,candidate:args[0]},args[1]),
  getTask:(task,context)=>call("runtimeGetTask",{runtime:id,task},context),
  ownedTasks:context=>call("runtimeOwnedTasks",{runtime:id},context),
  waitForTask:(task,context)=>call("runtimeWaitForTask",{runtime:id,task},context),
  outcomes:(tasks,context)=>call("outcomes",{runtime:id,tasks},context),
  ...documents({runtime:id}),
  context:(conversation,context,options)=>call("runtimeContext",{runtime:id,conversation,at:options?.at},context),
  conversation:(conversation,context)=>call("runtimeConversation",{runtime:id,conversation},context).then(descriptor=>descriptor===undefined?undefined:boundConversation(descriptor)),
  now:()=>nativeTransport.sync("runtimeNow",{runtime:id}),
  report:(failure,scope)=>nativeTransport.sync("runtimeReport",{runtime:id,error:nativeTransport.encodeError(failure),scope}),
  sleepUntil:(until,checkpoint,context)=>call("sleepUntil",{runtime:id,until:typeof until==="number"&&!Number.isFinite(until)?String(until):until,checkpoint},context),
 };
 runtimes.set(id,held);return held;
}
async function invoke(method,args,context){
 if(method==="phase"){
  stats.phases++;const[id,phase,task,runtimeId,_serial,harnessId]=args;nativeTransport.actorRuntime(runtimeId,harnessId);const current=runtime(runtimeId);current.taskId=task.id;current.conversationId=task.conversationId;current.harness=harnessId;
  const callContext=withAbortSignal(current.signal,context??BACKGROUND_CONTEXT);const definition=objectFor(id).definition;
  // Native admission can be sealed while this callback crosses the pipe. A
  // handler that has not entered yet must not start under an ended invocation.
  if(current.signal.aborted||phase==="$abort"&&!nativeTransport.sync("phase.enter",{runtime:runtimeId,harness:harnessId}))return;
  return (phase==="$abort"?definition.abort:definition.phases[phase])(task,current,callContext);
 }
 if(method==="change")return captureChange(args.tx,tx=>callbacks.get(args.callback)(tx,args.current??undefined));
 if(method==="storage"){
  stats.storageCalls++;const storage=storages.get(args.handle);let values=args.args;
  if(args.method==="commit"){
   const[writes,adoption,identities]=values;for(const identity of identities??[]){const content=writes[identity.index].content;if(identity.value!==undefined)content.value=graphValue(identity.value);if(identity.ops!==undefined)content.ops=graphValue(identity.ops);}return storage.commit(writes.map(tableWrite),seq=>nativeTransport.sync("storage.adopt",{adoption,seq}),context);
  }
  // JSON null is used only for omitted optional cursor/cutoff arguments here.
  if(args.method.startsWith("scan")||args.method==="findLatestHeadMarker")values=values.map(value=>value===null?undefined:value);
  return storage[args.method](...values,context);
 }
 throw new Error(`Unknown native callback ${method}`);
}
async function captureChange(tx,callback){
  try{
   const result=await callback(transaction(tx));
   // JS captures edits to its proxy. Rust owns replay, validation, prepare,
   // persistence, adoption and publication of every document revision.
   for(const [draft,held] of drafts.get(tx)??[]){const prepared=held.change.prepare();await call("doc.apply",{draft,ops:prepared.ops});}
   return result;
  }finally{if(drafts.has(tx)){for(const held of drafts.get(tx).values())held.change.abort();drafts.delete(tx);nativeTransport.sync("doc.release",{tx:tx});}}
}
export const Harness={async open(storage,options,context){
 const storageId=storageNext++;storages.set(storageId,storage);
 const registry=nativeRegistryId(options.registry);const reader=registry===undefined?handle(options.registry):undefined;
 const expected=nativeTransport.nextId;harnesses.set(expected,{options,onReport:options.onReport,storage});
 const id=await call("open",{rootTelemetry:describeTelemetry(options.telemetry),storage:storageId,nativeMemory:nativeMemoryHandle(storage),nativeSqlite:nativeSqliteHandle(storage),readOnly:options.readOnly===true,onWake:options.onWake!==undefined,setTimer:options.setTimer!==undefined,conversationCreated:options.conversationCreated!==undefined,nativeGeneration:process.env.PI_DURABLE_NATIVE_GENERATION_TASK==="1",registry,reader,environment:options.env!==undefined,clock:options.now!==undefined,settings:options.settings!==undefined,retention:getDefaultDocumentRetention()},context);
 let closed;
 const observeClosed=()=>closed??=call("closed",{harness:id}).then(end=>end.error?{...end,error:error(end.error)}:end);
 const modelsOption=options.models;
 const identity=mergeClientIdentity(DEFAULT_HARNESS_IDENTITY,options.identity);
 const fixedModels=modelsOption&&typeof modelsOption!=="function"?withClientIdentity(modelsOption,identity):undefined;
 const harness={observeCommits:callback=>{const subscription=nativeTransport.sync("view.observeCommits",{harness:id,callback:handle(callback)});return()=>nativeTransport.sync("view.unsubscribeCommits",{subscription});},subscribeClose:callback=>{const subscription=nativeTransport.sync("view.subscribeClose",{harness:id,callback:handle(callback)});return()=>nativeTransport.sync("view.unsubscribeCommits",{subscription});},subscribeCommits:callback=>{const subscription=nativeTransport.sync("view.subscribeCommits",{harness:id,callback:handle(callback)});return()=>nativeTransport.sync("view.unsubscribeCommits",{subscription});},options,onReport:options.onReport,storage,currentModels:()=>fixedModels??(typeof modelsOption==="function"?withClientIdentity(modelsOption(),identity):undefined),
  submission:(submission,context)=>call("submission.get",{harness:id,submission},context).then(submission=>submission===undefined?undefined:submissionHandle(submission)),
  abortSubmission:(submission,context,conversation)=>call("submission.abort",{harness:id,submission,conversation},context),
  wake:()=>nativeTransport.sync("wake",{harness:id}),
  inspect:context=>call("inspect",{harness:id},context).then(inspect),
  watchTaskGraph:context=>call("graph.watch",{harness:id},context).then(graphWatch),
  ...documents({harness:id}),
  conversation:async(idToFind,context)=>{const record=await call("conversation",{harness:id,conversation:idToFind},context);return record?conversation(record.id):undefined;},
  root:async(context,options)=>conversation(await call("root",{harness:id,options:conversationOptions(options)},context)),
  resume:()=>{nativeTransport.sync("resume",{harness:id});},
  createConversation:async(options,context)=>conversation(await call("createConversation",{harness:id,options:conversationOptions(options)},context)),
  waitForIdle:context=>call("waitForIdle",{harness:id,conversation:null},context),
  getTask:(task,context)=>call("getTask",{harness:id,task},context),
  retryTask:(task,context)=>call("retryTask",{harness:id,task},context),
  usage:context=>call("usage",{harness:id},context),
  waitForTask:(task,context)=>call("waitForTask",{harness:id,task},context),
  abortTask:(task,context)=>call("abortTask",{harness:id,task},context),
  commit:(callback,context)=>change("commit",{harness:id,conversation:null},callback,context),
  commitWith:(callback,context)=>change("commit",{harness:id,conversation:null},callback,context),
  get closed(){return observeClosed();},close:context=>nativeTransport.requestAdmitted("close",{harness:id},context),
  unloadDocuments:()=>call("unloadDocuments",{harness:id}),
  readOnLine:callback=>readSessionLine(id,callback),
  get documentCacheSizes(){return nativeTransport.sync("session.cache",{harness:id});},
  get [Symbol.for("pi-durable.schedulerIndexSizes")](){return nativeTransport.sync("indexSizes",{harness:id});},
 };
 function submissionHandle(submission){return {id:submission,status:context=>call("submission.status",{harness:id,submission},context),wait:context=>call("submission.wait",{harness:id,submission},context),abort:context=>call("submission.abort",{harness:id,submission,handle:true},context)};}
 function conversation(conversationId){return{id:conversationId,compact:(instructions,context)=>call("conversation.compact",{harness:id,conversation:conversationId,instructions},context),context:(context,options)=>call("conversationContext",{harness:id,conversation:conversationId,at:options?.at},context),fork:(at,options,context)=>call("forkConversation",{harness:id,conversation:conversationId,at,options:conversationOptions(options)},context).then(conversation),reset:async(handoff,context)=>{const entry={kind:"pi.reset",head:"self"};if(handoff!==undefined)entry.model=[{role:"user",content:handoff,timestamp:options.now?.()??Date.now()}];await call("submission.submit",{harness:id,conversation:conversationId,draft:{type:"write",entry}},context);},watch:context=>call("view.watch",{harness:id,conversation:conversationId},context).then(graphWatch),abort:(context,options)=>call("conversation.abort",{harness:id,conversation:conversationId,background:options?.background},context),agent:context=>call("conversationAgent",{harness:id,conversation:conversationId},context).then(hydrateAgent),submit:(draft,context)=>call("submission.submit",{harness:id,conversation:conversationId,draft},context).then(submissionHandle),entries:(query,limit,cursor,context)=>call("entries",{harness:id,conversation:conversationId,query,limit,cursor},context),configure:(change,context)=>call("configure",{harness:id,conversation:conversationId,change:configuration(change)},context),waitForIdle:context=>call("waitForIdle",{harness:id,conversation:conversationId},context),commit:(callback,context)=>change("commit",{harness:id,conversation:conversationId},callback,context)};}
 viewBindings.set(harness,id);harnesses.set(id,harness);trackCrashFixture(storage,id);return harness;
}};
const storageIds=new WeakMap();let shuttingDown=false;
export const transport={request:call,storage(storage){let id=storageIds.get(storage);if(id===undefined){id=storageNext++;storageIds.set(storage,id);storages.set(id,storage);}return id;},release(range){if(!shuttingDown&&nativeTransport.started)send({type:"context.release",range});}};
async function finish(contextOnly,submissionsOnly=false,checkpointPolicies=false){
 shuttingDown=true;
 if(!nativeTransport.started)throw new Error("Stock task tests did not reach Rust");
 const native=await call("stats");
 if(checkpointPolicies){if(native.nativeCodemodeCheckpointPolicies===0)throw new Error("Native checkpoint policies were bypassed");}
 else if(contextOnly){if(native.contextReads===0||stats.storageCalls===0)throw new Error("Native context implementation was bypassed");}
 else if(submissionsOnly){if(native.submissionAdmissions===0||native.submissionReads===0||native.submissionWaits===0||stats.storageCalls===0)throw new Error("Native submissions were bypassed");}
 else if(native.phases!==stats.phases||(native.phases===0&&native.nativeModelRequests===0&&native.nativeHarnessOpens===0)||(native.commits===0&&native.nativeMemoryCommits===0&&native.nativeSqliteCommits===0)||(stats.storageCalls===0&&native.nativeMemoryCalls===0&&native.nativeSqliteCalls===0))throw new Error("Native scheduler was bypassed");
 if(process.env.PI_DURABLE_NATIVE_DUMP_STATE)writeFileSync(process.env.PI_DURABLE_NATIVE_DUMP_STATE,JSON.stringify(await call("diagnostics"),null,2));
 Object.assign(stats,native);
 if(process.env.PI_DURABLE_NATIVE_TASK_STATS)writeFileSync(process.env.PI_DURABLE_NATIVE_TASK_STATS,JSON.stringify(stats));
 await nativeTransport.close();
}
export async function shutdown(){await finish(false);}
export async function shutdownContextTransport(){await finish(true);}
export async function shutdownSubmissions(){await finish(false,true);}
export async function shutdownCodemodePolicies(){await finish(false,false,true);}

const toolApiStates=new WeakMap();
export function invokeCodemodeGlobal(args,context){const api=toolApi(args.api),global=objectFor(args.callback);return global.execute(args.present?args.args:undefined,api,withAbortSignal(runtime(args.api.runtime).signal,context??BACKGROUND_CONTEXT));}
function toolApi(args){
 const state={ended:false},details=[];let cursor=0;
 const drain=()=>{while(cursor<details.length&&details[cursor].outcome){const slot=details[cursor++];if(slot.outcome==="ok")slot.resolve();else if(slot.outcome==="error")slot.reject(slot.error);}};
 const current=runtime(args.runtime);current.taskId=args.taskId;current.conversationId=args.conversationId;current.harness=args.harness;
 const bound={...current,commit:(callback,context)=>change("tool.commit",{api:args.api},callback,context),createTask:(task,input,options,context)=>call("tool.createTask",{api:args.api,definition:describeTask(task),input,options},context),callId:args.callId,outputWindow:args.outputWindow??undefined,env:objectFor(args.environment),
  output:(chunk,skipped)=>nativeTransport.sync("tool.output",{api:args.api,...(typeof chunk==="string"?{text:chunk}:{bytes:Buffer.from(chunk).toString("base64")}),skipped}),
  diagnostic:diagnostic=>nativeTransport.sync("tool.diagnostic",{api:args.api,diagnostic}),
  details:(value,context)=>{
   const afterEnd=state.ended;
   return new Promise((resolve,reject)=>{
    const slot={resolve,reject};details.push(slot);
    call("tool.details",{api:args.api,value},context).then(()=>{slot.outcome="ok";drain();},error=>{
     // Cancellations and calls made after execute are independent waits; a
     // shared progress/final commit settles its other marks in admission order.
     if(afterEnd||context?.abortSignal?.aborted||nativeTransport.isCancellation(error)||nativeTransport.isAdmissionFailure(error)){slot.outcome="rejected";reject(error);}else{slot.outcome="error";slot.error=error;}
     drain();
    });
   });
  },
  retainedOutput:()=>nativeTransport.sync("tool.retained",{api:args.api}),
  executeTool:(name,toolArgs,context,options={})=>call("tool.nested",{api:args.api,name,arguments:toolArgs,options},context),
 };
 Object.defineProperty(bound,"models",{get:()=>current.models});
 toolApiStates.set(bound,state);
 return bound;
}

const nativeOutputArrays=new Map(),nativeOutputRefs=new WeakMap();
function packToolResult(result){
 const plain={...result},values={};
 for(const key of ["details","structuredOutput"])if(result[key]!==undefined){try{plain[key]=copyJson(result[key],{omitUndefinedProperties:true});}catch{values[key]=valueHandle(result[key]);delete plain[key];}}
 return {result:plain,values,contentHost:result.output&&handle(result.output),contentRef:result.output&&nativeOutputRefs.get(result.output)};
}
function unpackToolResult(wire){
 const result=wire.result;
 for(const [key,id] of Object.entries(wire.values??{}))result[key]=objectFor(id);
 if(wire.contentHost!==undefined)result.output=objectFor(wire.contentHost);
 else if(wire.contentRef!==undefined&&nativeOutputArrays.has(wire.contentRef))result.output=nativeOutputArrays.get(wire.contentRef);
 if(result.output&&wire.contentRef!==undefined){nativeOutputArrays.set(wire.contentRef,result.output);nativeOutputRefs.set(result.output,wire.contentRef);}
 return result;
}
function hookRuntime(args){const current=runtime(args.runtime);current.taskId=args.taskId;current.conversationId=args.conversationId;current.harness=args.harness;return current;}
nativeTransport.register("tool.beforeHook",(args,context)=>objectFor(args.handler)(args.call,hookRuntime(args),withAbortSignal(runtime(args.runtime).signal,context??BACKGROUND_CONTEXT)));
nativeTransport.register("tool.afterHook",async(args,context)=>{const result=await objectFor(args.handler)(args.call,unpackToolResult(args.result),hookRuntime(args),withAbortSignal(runtime(args.runtime).signal,context??BACKGROUND_CONTEXT));return result===undefined?undefined:packToolResult(result);});

nativeTransport.register("environment.build",async(args,context)=>{
 const harness=harnesses.get(args.harness),build=harness.options.env;if(build===undefined)return undefined;
 const target={conversationId:args.conversationId,read:harness};if(args.cwd!==null)target.cwd=args.cwd;
 const env=await build(target,context??BACKGROUND_CONTEXT);return env===undefined?undefined:{handle:handle(env),namespace:env.id,cwd:env.cwd};
});
nativeTransport.register("environment.call",async(args,context)=>{
 const scope=environmentContext(args,context??BACKGROUND_CONTEXT),callbackContext=scope.context;
 try{
 const env=objectFor(args.environment),values=[...args.args];
 if(args.method==="writeTextFile")return await env.writeFile(...values,callbackContext);
 if(args.method==="writeFile"||args.method==="appendFile")values[1]=Uint8Array.from(Buffer.from(values[1],"base64"));
 if(args.method==="exec"&&values[1]?.onOutput!==undefined){const callback=values[1].onOutput;values[1].onOutput=(text,_context,info)=>nativeTransport.sync("environment.output",{callback,text,info});}
 const result=await env[args.method](...values,callbackContext);
 if(args.method==="cleanup")return {ok:true,value:null};
 if(result.ok){if(args.method==="readBinaryFile")return {ok:true,value:Buffer.from(result.value).toString("base64")};return result;}
 return {ok:false,error:{code:result.error.code,message:result.error.message,path:result.error.path,spillPath:result.error.spillPath}};
 }finally{scope.release();}
});

nativeTransport.register("tool.copyValue",id=>copyJson(objectFor(id),{omitUndefinedProperties:true}));

const commitListenerPromises=new Map();
function listenerResult(result){if(result&&typeof result.then==="function"){const promise=Promise.resolve(result);promise.catch(()=>{});const capability=temporaryHandle(promise);commitListenerPromises.set(capability.id,capability);return {pending:capability.id};}return {};}
function publicationValue(publication){return {...publication,changes:publication.changes.map(({nativeValue,nativeOps,...change})=>{if(nativeValue!==undefined)change.value=graphValue(nativeValue);if(nativeOps!==undefined)change.ops=graphValue(nativeOps);return tableWrite(change);})};}
nativeTransport.register("commit.listenerResult",id=>{const capability=commitListenerPromises.get(id);commitListenerPromises.delete(id);return objectFor(id).finally(capability.release);});
nativeTransport.register("commit.publication",args=>listenerResult(objectFor(args.callback)(publicationValue(args.publication),BACKGROUND_CONTEXT)));

nativeTransport.register("view.create",async args=>handle(await objectFor(args.callback)(graphValue(args.value),()=>nativeTransport.sync("view.detachObserver",{attachment:args.attachment}))));
nativeTransport.register("view.publication",args=>{objectFor(args.callback).publication?.(graphValue(args.before),graphValue(args.after),args.ops,publicationValue(args.publication),BACKGROUND_CONTEXT);return null;});
nativeTransport.register("view.end",args=>{objectFor(args.callback).end(args.failure===null?undefined:{error:error(args.failure)});return null;});
export function conversationViews(harness){
 const id=viewBindings.get(harness);if(id===undefined)throw new Error("Not a Harness");
 return {attach:async(conversation,create,context)=>{const factory=temporaryHandle(create);try{const result=await call("view.attach",{harness:id,conversation,callback:factory.id},context);return {observer:objectFor(result.observer),detach:()=>nativeTransport.sync("view.detachObserver",{attachment:result.attachment})};}finally{factory.release();}}};
}

export async function watchEvents(harness,conversation,context){
 const id=viewBindings.get(harness);if(id===undefined)throw new Error("Not a Harness");
 const descriptor=await call("view.events",{harness:id,conversation},context),watch=graphWatch(descriptor);
 return {snapshot:descriptor.eventSnapshot,start:listener=>watch.start((events,_ops,delivery)=>listener(events,delivery)),stop:()=>watch.stop(),closed:watch.closed};
}

const modelControllers=new Map();
nativeTransport.event("models.signal",({stream})=>modelControllers.get(stream)?.abort(new Error("Aborted")));
nativeTransport.register("models.collection",id=>handle(harnesses.get(id).currentModels()));
nativeTransport.register("models.get",args=>{const model=objectFor(args.collection).getModel(args.reference.provider,args.reference.modelId);return model===undefined?null:handle(model);});
nativeTransport.register("models.metadata",args=>{const value=objectFor(args.model)[args.property];return Number.isFinite(value)?value:String(value);});
nativeTransport.register("models.imageInput",id=>{const model=objectFor(id),resize=model?.inputLimits?.images?.resize,scalar=value=>typeof value==="number"&&!Number.isFinite(value)?String(value):value;return {vision:model?.input.includes("image")??true,maxWidth:scalar(resize?.maxWidth),maxHeight:scalar(resize?.maxHeight),maxBytes:scalar(resize?.maxBytes)};});
nativeTransport.register("models.field",args=>{const value=objectFor(args.message)[args.name];return value===undefined?{present:false}:{present:true,value:copyJson(value,{omitUndefinedProperties:true})};});
nativeTransport.register("models.setField",args=>{if(args.present)objectFor(args.message)[args.name]=args.value;else delete objectFor(args.message)[args.name];return null;});
nativeTransport.register("models.copy",id=>copyJson(objectFor(id),{omitUndefinedProperties:true}));
nativeTransport.register("models.hasContent",id=>objectFor(id).content.length!==0);
nativeTransport.register("models.toolCalls",id=>copyJson(objectFor(id).content.filter(block=>block.type==="toolCall"),{omitUndefinedProperties:true}));
nativeTransport.register("models.hasToolCalls",id=>objectFor(id).content.some(block=>block.type==="toolCall"));
nativeTransport.register("models.textBlocks",id=>objectFor(id).content.flatMap(block=>block.type==="text"?[block.text]:[]));
nativeTransport.register("models.capture",value=>handle(value));
nativeTransport.register("models.request",async(args,context)=>{
 const controller=new AbortController();modelControllers.set(args.stream,controller);if(args.aborted)controller.abort(new Error("Aborted"));
 try{
  const models=objectFor(args.collection),options={...args.options,signal:controller.signal,telemetry:context?.telemetry},model=objectFor(args.model);
  if(args.method==="fetchDeferred")return handle(await models.fetchDeferred(model,args.request,options));
  if(args.method==="completeSimple")return handle(await models.completeSimple(model,args.request,options));
  const stream=models.streamSimple(model,args.request,options);
  for await(const event of stream){if(event.type!=="done"&&event.type!=="error")nativeTransport.notify({type:"models.event",stream:args.stream,message:handle(event.partial)});}
  return handle(await stream.result());
 }finally{modelControllers.delete(args.stream);}
});
nativeTransport.register("models.cancel",async args=>{
 const controller=new AbortController();modelControllers.set(args.stream,controller);if(args.aborted)controller.abort(new Error("Aborted"));
 try{await objectFor(args.collection).cancelDeferred(objectFor(args.model),args.handle,{signal:controller.signal});return null;}finally{modelControllers.delete(args.stream);}
});
function generationRuntime(args){const current=runtime(args.runtime);current.harness=args.harness;current.taskId=args.taskId;current.conversationId=args.conversationId;return current;}
nativeTransport.register("generation.render",(args,context)=>{const read=generationRuntime(args);return objectFor(args.section).render({conversationId:args.conversationId,agent:hydrateAgent(args.agent),env:objectFor(args.environment),shown:Object.fromEntries(args.shown),read},withAbortSignal(read.signal,context??BACKGROUND_CONTEXT));});

function conversationOptions(options){if(options===undefined)return undefined;return {...options,agent:options.agent===undefined?undefined:configuration(options.agent),init:options.init===undefined?undefined:handle(options.init)};}
nativeTransport.register("conversation.init",async args=>{
 await captureChange(args.tx,tx=>objectFor(args.callback)(tx,args.conversation));
 return null;
});

for(const method of ["beforeRequest","afterResponse","onYield","afterTools"]){
 nativeTransport.register(`generation.${method}`,(args,context)=>{
  const api=generationRuntime(args),callback=objectFor(args.callback),callContext=withAbortSignal(api.signal,context??BACKGROUND_CONTEXT);
  if(method==="beforeRequest")return callback({messages:args.messages},api,callContext);
  if(method==="afterTools")return callback(args.assistant,args.results,api,callContext);
  return callback(objectFor(args.message),api,callContext);
 });
}

const compactionSources=new WeakMap();
nativeTransport.register("compactionHook",async(args,context)=>{
 const api=generationRuntime(args);if(!compactionSources.has(api))compactionSources.set(api,args.source);
 const result=await objectFor(args.callback)(compactionSources.get(api),api,withAbortSignal(api.signal,context??BACKGROUND_CONTEXT));
 return result===undefined?undefined:"decline" in result?{decline:true}:{summary:result.summary};
});

// Pure Session transport. It never creates a Harness or starts a scheduler.
const sessions=new Map();
nativeTransport.register("session.readLine",async callback=>({hostResult:valueHandle(await objectFor(callback)())}));
function readSessionLine(id,callback){
 const capability=temporaryHandle(callback);
 const operation=call("session.readLine",{harness:id,callback:capability.id}).then(result=>objectFor(result.hostResult)).finally(capability.release);
 operation.catch(()=>{});return operation;
}
nativeTransport.register("session.now",id=>sessions.get(id).now());
nativeTransport.register("session.retention",id=>{const value=sessions.get(id).retention();return typeof value==="number"&&!Number.isFinite(value)?String(value):value??null;});
nativeTransport.register("session.conversationCreated",args=>captureChange(args.tx,tx=>sessions.get(args.harness).session.deref().conversationCreated(tx,args.record)));
nativeTransport.register("session.closeListener",id=>listenerResult(objectFor(id)()));
nativeTransport.register("session.beforeClose",id=>sessions.get(id).session.deref().beforeClose());
installSessionBinding((session,storage,now,retention,setTimer,readOnly)=>{
 const storageId=storageNext++;storages.set(storageId,storage);
 const expected=nativeTransport.nextId;sessions.set(expected,{session:new WeakRef(session),now,retention,setTimer});
 const transactionProbes=process.env.PI_DURABLE_NATIVE_TRANSACTION_PROBES==="1";
 const id=nativeTransport.sync("session.open",{storage:storageId,nativeMemory:transactionProbes?undefined:nativeMemoryHandle(storage),readOnly,hostTimers:setTimer!==undefined,transactionProbes});
 let closed;
 return {
  ...documents({harness:id},session),
  commitWith:(callback,context,scope)=>change("commit",{harness:id,conversation:scope?.conversationId??null},async tx=>({hostResult:valueHandle(await callback(tx))}),context).then(result=>objectFor(result.hostResult)),
  subscribeCommits:callback=>{const subscription=nativeTransport.sync("view.subscribeCommits",{harness:id,callback:handle(callback)});return()=>nativeTransport.sync("view.unsubscribeCommits",{subscription});},
  subscribeClose:callback=>{const subscription=nativeTransport.sync("view.subscribeClose",{harness:id,callback:handle(callback)});return()=>nativeTransport.sync("view.unsubscribeCommits",{subscription});},
  closed:()=>closed??=call("closed",{harness:id}).then(end=>end.error?{...end,error:error(end.error)}:end),
  close:context=>nativeTransport.requestAdmitted("close",{harness:id},context),
  unloadDocuments:()=>call("unloadDocuments",{harness:id}),
  readOnLine:callback=>readSessionLine(id,callback),
  cache:()=>nativeTransport.sync("session.cache",{harness:id}),
 };
});
export async function shutdownSessions(){
 const native=await call("stats");
 const mixed=process.env.PI_DURABLE_NATIVE_SESSION_HARNESS==="1";
 if(native.nativeSessionOpens===0||(!mixed&&native.nativeHarnessOpens!==0)||native.phases!==0)throw new Error("Stock Session tests bypassed the native kernel or dispatched source phases");
 if(mixed&&(native.nativeHarnessOpens===0||native.nativeModelRequests===0||native.nativeToolCalls===0))throw new Error("Mixed Session/Harness stock tests bypassed native services");
 if(process.env.PI_DURABLE_NATIVE_TASK_STATS)writeFileSync(process.env.PI_DURABLE_NATIVE_TASK_STATS,JSON.stringify({...stats,...native}));
 await nativeTransport.close();
}
