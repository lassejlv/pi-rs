// Test-only handles preserve the identity of original JS registrations. All
// registry decisions and agent composition execute in the Rust process.
import {writeFileSync} from "node:fs";
import {GenerationTask,ToolTask,CompactionTask} from "@earendil-works/pi-durable";
import {nativeTransport,failure} from "./durable-native-transport.mjs";
import {captureSettings,settingsRequest,hydrateSettings} from "./durable-settings-wire.mjs";
import {handle,objectFor,describeTask,markNativeTool,markNativeGeneration,markNativeCompaction} from "./durable-native-handles.mjs";
if(process.env.PI_DURABLE_NATIVE_COMPACTION_TASK==="1")markNativeCompaction(CompactionTask);
if(process.env.PI_DURABLE_NATIVE_GENERATION_TASK==="1")markNativeGeneration(GenerationTask);
if(process.env.PI_DURABLE_NATIVE_TOOL_TASK==="1")markNativeTool(ToolTask);
const ids=new WeakMap(),snapshots=new Map(),agents=new WeakMap(),resolvedAgents=new Map(),registries=new WeakMap();
const callbacks={listeners:0,wrappers:0,reports:0};
const call=(method,args={})=>nativeTransport.sync(`registry.${method}`,args);
function describeTool(tool){return{id:handle(tool),name:tool.name,nativeBuiltin:tool.__piNativeBuiltin,codemodeOptions:tool.__piCodemodeOptions,callers:tool.callers,deferred:tool.deferred===true,...(process.env.PI_DURABLE_NATIVE_TOOL_TASK==="1"?{declaration:{name:tool.name,description:tool.description,parameters:tool.parameters,constrainedSampling:tool.constrainedSampling},replay:tool.replay,executionMode:tool.executionMode,limits:{maxBytes:tool.outputLimits?.maxBytes??51200,maxLines:tool.outputLimits?.maxLines??2000,retain:tool.outputLimits?.retain??"head"},schema:tool.structuredOutputSchema}:{})};}
function describeSection(section){return{id:handle(section),key:section.key,untagged:section.tag===false};}
function describeExtension(extension){return{
 id:handle(extension),name:extension.name,tools:(extension.tools??[]).map(describeTool),sections:(extension.sections??[]).map(describeSection),tasks:(extension.tasks??[]).map(describeTask),
 hooks:(extension.hooks??[]).map(hook=>({task:hook.task,handlers:handle(hook.handlers),nativeTool:process.env.PI_DURABLE_NATIVE_TOOL_TASK==="1"&&hook.task==="pi.tool",nativeGeneration:process.env.PI_DURABLE_NATIVE_GENERATION_TASK==="1"&&hook.task==="pi.generation",nativeCompaction:process.env.PI_DURABLE_NATIVE_COMPACTION_TASK==="1"&&hook.task==="pi.compaction"})),
 wraps:(extension.wraps??[]).map(wrap=>({...(Object.hasOwn(wrap,"tool")?{tool:wrap.tool}:{section:wrap.section}),callback:handle(wrap.wrap)})),
};}
nativeTransport.register("registry.listener",args=>{callbacks.listeners++;objectFor(args)();return null;});
nativeTransport.register("registry.wrap",args=>{callbacks.wrappers++;const result=objectFor(args.callback)(objectFor(args.item));return args.kind==="tool"?describeTool(result):describeSection(result);});
nativeTransport.register("registry.report",args=>{callbacks.reports++;objectFor(args.callback)(failure(args.error),args.extension);return null;});
export function createRegistry(){
 const registry=call("create",{builtins:[GenerationTask,ToolTask,CompactionTask].map(describeTask)});
 const result={
  snapshot:()=>snapshot(call("snapshot",{registry})),
  install:extension=>{call("install",{registry,extension:describeExtension(extension)});},
  uninstall:extension=>{call("uninstall",{registry,name:extension.name});},
  subscribe:listener=>{const subscription=call("subscribe",{registry,callback:handle(listener)});return()=>{call("unsubscribe",{subscription});};},
 };
 registries.set(result,registry);return result;
}
export function nativeRegistryId(registry){return registries.get(registry);}
export function nativeSnapshotId(value){const id=ids.get(value);if(id===undefined)throw new Error("Custom reader must return a native registry snapshot");return id;}
export function lookupNativeSnapshot(value){return ids.get(value);}
export function snapshot(id){
 let held=snapshots.get(id);if(held)return held;
 const request=(method,args={})=>call(method,{snapshot:id,...args});
 held={installed:()=>request("installed").map(id=>objectFor(id)),extension:name=>objectFor(request("extension",{name})),
  tools:()=>request("tools").map(row=>({extension:objectFor(row.extension),tool:objectFor(row.tool)})),
  sections:()=>request("sections").map(row=>({extension:objectFor(row.extension),section:objectFor(row.section)})),
  tasks:()=>request("tasks").map(id=>objectFor(id)),task:name=>objectFor(request("task",{name})),
 };
 snapshots.set(id,held);ids.set(held,id);return held;
}
export function resolveSettings(settings){
 const {request,extensions}=captureSettings(settings);const result=hydrateSettings(call("settings",request));
 if(extensions!==undefined)result.extensions=extensions;return result;
}
export function resolveAgent(state,snapshot,settings,report){
 return hydrateAgent(call("resolve",{snapshot:ids.get(snapshot),state:state??null,...settingsRequest(settings),report:handle(report)}));
}
export function hydrateAgent(result){
 const {id,...fields}=result;const held=resolvedAgents.get(id);if(held)return held;
 const agent={...fields,extensions:fields.extensions.map(id=>objectFor(id)),tools:fields.tools.map(id=>objectFor(id)),callable:fields.callable.map(id=>objectFor(id)),deferred:fields.deferred.map(id=>objectFor(id)),
  sections:fields.sections.map(section=>Object.hasOwn(section,"id")?objectFor(section.id):{key:"instructions",render:()=>section.instructions}),
 };
 agents.set(agent,id);resolvedAgents.set(id,agent);return agent;
}
export function agentHooks(agent,task){return call("hooks",{agent:agents.get(agent),task}).map(hook=>({...hook,handlers:objectFor(hook.handlers)}));}
export function sectionExtension(agent,key){return call("sectionExtension",{agent:agents.get(agent),key});}
export function toolExtension(agent,name){return call("toolExtension",{agent:agents.get(agent),name});}
export async function shutdownRegistry(){
 if(!nativeTransport.started)throw new Error("Stock registry tests did not reach Rust");
 const native=call("stats");
 if(native.operations===0||native.resolutions===0||native.wrappers===0||callbacks.listeners===0||callbacks.wrappers!==native.wrappers)throw new Error("Native registry or callback execution was bypassed");
 await nativeTransport.close();
 if(process.env.PI_DURABLE_NATIVE_REGISTRY_STATS)writeFileSync(process.env.PI_DURABLE_NATIVE_REGISTRY_STATS,JSON.stringify({...native,callbacks}));
}
