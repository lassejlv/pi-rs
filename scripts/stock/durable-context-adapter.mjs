// Serialization/admission transport for the original context tests. All range,
// edit, baseline, incremental and tool-result decisions execute in Rust.
import {execFileSync} from "node:child_process";
import {transport,shutdownContextTransport} from "./durable-task-adapter.mjs";
const leases=new FinalizationRegistry(handle=>transport.release(handle));
const args=(storage,conversation)=>({storage:transport.storage(storage),conversation});
export function captureContextBounds(storage,conversation,context,at){return transport.request("context.capture",{...args(storage,conversation),at:at??null},context);}
function hydrate(encoded){const view=encoded.value;view.messages=view.messages.map((message,index)=>{const alias=encoded.messageContributions[index];return alias===null?message:view.contributions[alias[0]][alias[1]];});return view;}
export async function deriveContext(storage,conversation,bounds,context){return hydrate(await transport.request("context.derive",{...args(storage,conversation),bounds:bounds??null},context));}
export function activeEntries(storage,conversation,bounds,context){return transport.request("context.active",{...args(storage,conversation),bounds:bounds??null},context);}
export async function readContext(session,storage,conversation,context,at){
 const bounds=await session.readOnLine(()=>captureContextBounds(storage,conversation,context,at));
 return deriveContext(storage,conversation,bounds,context);
}
function freeze(value){if(value!==null&&typeof value==="object"&&!Object.isFrozen(value)){for(const child of Object.values(value))freeze(child);Object.freeze(value);}return value;}
export async function readContextFrom(session,storage,conversation,context,at,previous){
 const bounds=await session.readOnLine(()=>captureContextBounds(storage,conversation,context,at));
 const read=await transport.request("context.range",{...args(storage,conversation),bounds:bounds??null,previous:previous?.handle??null},context);
 read.view=hydrate(read.view);
 if(read.range===null)read.range=undefined;else{freeze(read.range.bounds);leases.register(read.range,read.range.handle);}
 freeze(read.view.head);for(const entry of read.view.entries)freeze(entry);for(const contribution of read.view.contributions)freeze(contribution);for(const message of read.view.messages)freeze(message);
 return read;
}
export function orderToolResults(messages){
 return JSON.parse(execFileSync(process.env.PI_DURABLE_NATIVE_TASK_BRIDGE,["--order-tool-results"],{input:JSON.stringify(messages),encoding:"utf8"}));
}
export async function shutdownContext(){await shutdownContextTransport();}
