// One native process serves synchronous registry calls and asynchronous task
// calls. A synchronous caller pumps frames so nested host callbacks can finish.
import {Worker,MessageChannel,receiveMessageOnPort,threadId} from "node:worker_threads";
import {appendFileSync} from "node:fs";
let worker,port,signal,next=1,closed=false,closing=false,transportFailure,exitResolve,synchronousCallbacks=0;
const pending=new Map(),handlers=new Map(),events=new Map(),completions=new Map(),cancellations=new WeakSet(),admissionFailures=new WeakSet();
const errors=new Map(),foreignErrors=new Map();let nextForeignError=1;
const contexts=new Map(),runtimeOwners=new Map(),abandoned=new Set();
function captureError(value){const id=nextForeignError++;foreignErrors.set(id,value);return id;}
function encodeError(error){
 let name="Error",message;
 try{if(typeof error?.name==="string")name=error.name;}catch{}
 try{message=error instanceof Error?error.message:String(error);}catch{try{message=Object.prototype.toString.call(error);}catch{message="[unformattable error]";}}
 return {foreignId:captureError(error),name,message};
}
let exited,contextAdapter;
const bufferedTrace=process.env.PI_DURABLE_NATIVE_TRACE_BUFFERED==="1"?[]:undefined;
function flushTrace(){if(bufferedTrace?.length&&process.env.PI_DURABLE_NATIVE_TRACE){appendFileSync(process.env.PI_DURABLE_NATIVE_TRACE,bufferedTrace.join(""));bufferedTrace.length=0;}}
process.once("exit",flushTrace);
export function failure(value){if(foreignErrors.has(value.foreignId))return foreignErrors.get(value.foreignId);const error=errors.has(value.kind)?errors.get(value.kind)(value):new Error(value.message);if(value.aborted)cancellations.add(error);if(value.name&&value.name!=="Error")error.name=value.name;if(value.cause)Object.defineProperty(error,"cause",{value:failure(value.cause),configurable:true,writable:true});return error;}
function trace(direction,frame){if(process.env.PI_DURABLE_NATIVE_TRACE){const row=JSON.stringify({at:performance.now(),pid:process.pid,threadId,direction,type:frame.type,id:frame.id,method:frame.method,operation:frame.args?.method,runtime:frame.runtime??frame.args?.runtime,callId:frame.args?.callId,run:frame.run??frame.args?.run,event:frame.event,...(frame.method==="codemode.vm.start"?{codePrefix:typeof frame.args?.code==="string"?frame.args.code.slice(0,256):undefined}:{}),error:frame.error??false,...(process.env.PI_DURABLE_NATIVE_TRACE_DETAIL==="1"?{frame}:{})})+"\n";if(bufferedTrace)bufferedTrace.push(row);else appendFileSync(process.env.PI_DURABLE_NATIVE_TRACE,row);}}
function send(frame){trace("send",frame);port.postMessage(frame);}
// Upstream runs a caller's continuation before native work that a later line job starts. Native
// steps wait until every frame sent before them has been acknowledged here, once the microtask
// queue has drained after the frame. A phase turn yields to native microtasks, so the gate stays
// open while one pumps; other synchronous requests (a change turn waits for its commit) keep it shut.
// Only a bridge that offers the gate (a leading host.acks frame) receives acknowledgements.
let received=0,ackScheduled=false,acks=false;
function acknowledge(){ackScheduled=false;if(acks&&worker&&!closing&&!closed)send({type:"host.ack",count:received});}
const setImmediateReal=globalThis.setImmediate;// captured now: a test may fake the timers later
function scheduleAck(){if(acks&&!ackScheduled){ackScheduled=true;setImmediateReal(acknowledge);}}
function fail(error){transportFailure=error;for(const [id,waiter] of pending){if(!waiter.sync){pending.delete(id);waiter.detach?.();waiter.reject(error);}}}
function dispatch(frame,delivered=false,held){
 if(!delivered&&frame.type!=="transport.error"&&frame.type!=="transport.exit"){received++;if(frame.type==="host.acks"){acks=true;trace("receive",frame);scheduleAck();return;}scheduleAck();}
 // MessagePort can deliver a batch before running promise continuations.
 // A callback after a reply must follow that reply's queued continuations,
 // including when a synchronous caller is pumping the batch itself.
 // Take the callback's context now: a later context.drop must not erase it before dispatch.
 if(!delivered&&frame.type==="callback"&&!frame.sync){const context=contexts.get(frame.context);queueMicrotask(()=>dispatch(frame,true,context));return;}
 trace("receive",frame);
 if(frame.type==="context.drop"){contexts.delete(frame.id);return;}
 if(frame.type==="admitted"){const waiter=pending.get(frame.id);if(waiter)waiter.admitted=true;return;}
 if(frame.type==="callback"){
  if(closing||closed)return;
  const handler=handlers.get(frame.method);
  const reject=error=>{if(closing||closed)return;send({type:"callback.result",id:frame.id,error:encodeError(error)});completions.get(frame.method)?.(frame);};
  const reply=value=>{if(closing||closed)return;try{send({type:"callback.result",id:frame.id,...(value===undefined?{missing:true}:{value})});}catch(error){reject(error);return;}completions.get(frame.method)?.(frame);};
  if(frame.sync)synchronousCallbacks++;
  try{
   if(!handler)throw new Error(`Unknown native callback ${frame.method}`);
   const context=held??contexts.get(frame.context);
   const value=handler(frame.args,contextAdapter?.decode(context,frame.telemetry)??context,frame);
   if(frame.sync){if(value&&typeof value.then==="function")throw new Error(`Synchronous callback returned a Promise: ${frame.method}`);reply(value);}
   else Promise.resolve(value).then(reply,reject);
  }catch(error){reject(error);}finally{if(frame.sync)synchronousCallbacks--;}
  return;
 }
 if(frame.type==="transport.error"){fail(new Error(frame.message));return;}
 if(frame.type==="transport.exit"){
  closed=true;exitResolve(frame.code);
  if(frame.code!==0||pending.size)fail(new Error(`Native durable bridge exited ${frame.code}`));return;
 }
 if(events.has(frame.type)){events.get(frame.type)(frame);return;}
 if(abandoned.delete(frame.id))return;
 const waiter=pending.get(frame.id);
 if(!waiter){fail(new Error(`Unknown native response ${frame.id}`));return;}
 if(waiter.sync){waiter.frame=frame;return;}
 pending.delete(frame.id);waiter.detach?.();frame.error?waiter.reject(failure(frame.error)):waiter.resolve(frame.missing?undefined:frame.value);
}
function start(){
 if(worker)return;
 const binary=process.env.PI_DURABLE_NATIVE_TASK_BRIDGE;if(!binary)throw new Error("Missing native durable bridge");
 const {port1,port2}=new MessageChannel();port=port1;signal=new Int32Array(new SharedArrayBuffer(4));exited=new Promise(resolve=>{exitResolve=resolve;});
 worker=new Worker(new URL("./durable-sync-worker.mjs",import.meta.url),{workerData:{port:port2,signal:signal.buffer,binary},transferList:[port2]});
 port.on("message",dispatch);worker.on("error",fail);
}
function drain(){for(let message;(message=receiveMessageOnPort(port));)dispatch(message.message);}
export const nativeTransport={
 encodeError,
 // Only explicit crash fixtures retire an actor this way. Its promises stay
 // unresolved, matching a lost process; every live actor still must settle all
 // requests before shutdown. Late replies from the retired actor are discarded.
 actorRuntime(runtime,owner){runtimeOwners.set(runtime,owner);},
 crashActor(owner){trace("fixture",{type:"actor.crash",harness:owner});for(const [id,waiter] of pending){if(waiter.owner===owner){pending.delete(id);waiter.detach?.();contexts.delete(id);abandoned.add(id);}}},
 contextAdapter(adapter){contextAdapter=adapter;},
 error(kind,create){if(errors.has(kind))throw new Error(`Duplicate native error ${kind}`);errors.set(kind,create);},
 isCancellation:error=>cancellations.has(error),isAdmissionFailure:error=>admissionFailures.has(error),
 get nextId(){return next;},get started(){return !!worker;},get pending(){return pending.size;},get inSynchronousCallback(){return synchronousCallbacks!==0;},
 afterCallback(method,handler){if(completions.has(method))throw new Error(`Duplicate native completion ${method}`);completions.set(method,handler);},
 register(method,handler){if(handlers.has(method))throw new Error(`Duplicate native handler ${method}`);handlers.set(method,handler);},
 event(type,handler){if(events.has(type))throw new Error(`Duplicate native event ${type}`);events.set(type,handler);},
 notify(frame){if(worker&&!closing)send(frame);},
 request(method,args={},context){
  start();const id=next++;
  if(context!==undefined)contexts.set(id,context);
  const response=new Promise((resolve,reject)=>{
   const signal=context?.abortSignal;const abort=()=>send({type:"abort",id,message:signal.reason?.message??String(signal.reason)});
   pending.set(id,{method,owner:args.harness??runtimeOwners.get(args.runtime),resolve,reject,context,detach:()=>signal?.removeEventListener("abort",abort)});
   try{send({id,method,...args,...(contextAdapter?{telemetry:contextAdapter.encode(context)}:{}),...(signal?.aborted?{abortInitially:signal.reason?.message??String(signal.reason)}:{})});}catch(error){pending.delete(id);admissionFailures.add(error);reject(error);return;}
   if(!signal?.aborted)signal?.addEventListener("abort",abort,{once:true});
  });
  // Native close can reject several waits before the held commit's reply
  // crosses the pipe. Observe transport failures immediately; callers still
  // receive this same rejecting promise and the original assertions see it.
  response.catch(()=>{});return response;
 },
 requestAdmitted(method,args={},context){
  const id=next,response=this.request(method,{...args,admit:true,reentrant:synchronousCallbacks!==0},context),waiter=pending.get(id),deadline=Date.now()+30000;
  while(pending.has(id)&&!waiter.admitted){
   const observed=Atomics.load(signal,0);drain();
   if(transportFailure)throw transportFailure;if(closed)throw new Error("Native durable transport closed");
   if(Date.now()>deadline)throw new Error(`Native durable admission timed out: ${method}`);
   if(pending.has(id)&&!waiter.admitted)Atomics.wait(signal,0,observed,10);
  }
  return response;
 },
 sync(method,args={}){
  start();const id=next++,waiter={sync:true,method};pending.set(id,waiter);const opens=acks&&method==="phase.turn",acked=received;if(opens)send({type:"host.ack",count:acked,open:true,runtime:args.runtime});send({id,method,...args});const deadline=Date.now()+30000;
  try{
   while(!waiter.frame){
    const observed=Atomics.load(signal,0);drain();
    if(transportFailure)throw transportFailure;if(closed)throw new Error("Native durable transport closed");
    if(Date.now()>deadline)throw new Error(`Native durable request timed out: ${method}`);
    if(!waiter.frame)Atomics.wait(signal,0,observed,10);
   }
   if(waiter.frame.error)throw failure(waiter.frame.error);return waiter.frame.missing?undefined:waiter.frame.value;
  }finally{pending.delete(id);if(opens&&!closing&&!closed)send({type:"host.ack",count:acked,open:false,runtime:args.runtime});scheduleAck();}
 },
 async close(){
  flushTrace();
  if(!worker)throw new Error("Stock tests did not reach Rust");
  if(pending.size)throw new Error(`Native requests still pending: ${pending.size} (${[...pending].map(([id,waiter])=>`${id}:${waiter.method}`).join(", ")})`);
  closing=true;send({type:"transport.close"});if(await exited!==0)throw new Error("Native durable bridge failed");
  port.close();await worker.terminate();flushTrace();
 },
};
