// Rust owns durable code-mode execution. Only the original VM and user callbacks cross IPC.
import {defineDoc} from "@earendil-works/pi-durable";
import {nativeTransport} from "./durable-native-transport.mjs";
import {handle,objectFor} from "./durable-native-handles.mjs";
import {createModelTools} from "./durable-codemode-models-adapter.mjs";
import {invokeCodemodeGlobal} from "./durable-task-adapter.mjs";
const runs=new Map();
export const CODEMODE_MODULE_VERSION=2,CODEMODE_TOOL_NAME="codemode",DEFAULT_CODEMODE_INLINE_BUDGET=3000;
const checkpoint=(kind,info)=>nativeTransport.sync("codemode.docCheckpoint",{kind,deltas:info.deltasSinceBase});
export const CodemodeRunDoc=defineDoc({kind:"pi.codemode.run",version:1,scope:"task",initial:()=>({}),checkpointWhen:(_v,_o,info)=>checkpoint("pi.codemode.run",info)});
export const CodemodeJournalDoc=defineDoc({kind:"pi.codemode.journal",version:1,scope:"task",initial:()=>({entries:[]}),checkpointWhen:(_v,_o,info)=>checkpoint("pi.codemode.journal",info)});
export const CodemodeStoreDoc=defineDoc({kind:"pi.codemode.store",version:1,scope:"conversation",history:"rewindable",fork:"asOf",initial:()=>({values:{}}),checkpointWhen:(_v,_o,info)=>checkpoint("pi.codemode.store",info)});
function optionsFor(options){return {sandbox:handle(options.sandbox),namespaces:options.namespaces,inlineBudget:options.inlineBudget,models:options.models,globals:(options.globals??[]).map(global=>({name:global.name,spread:global.spread===true,callback:handle(global)}))};}
export function createCodemodeTool(options){const wire=optionsFor(options);return {...nativeTransport.sync("codemode.declaration",{options:wire}),__piNativeBuiltin:"codemode",__piCodemodeOptions:wire,callers:["model"],replay:"safe",outputLimits:{maxBytes:4194304,maxLines:100000},execute(){throw new Error("Source code-mode replay driver was bypassed incorrectly");}};}
export function codemodeDescription(models){return nativeTransport.sync("codemode.description",{models});}
export function codemodeSection(tools,namespaces,budget){return nativeTransport.sync("codemode.section",{tools:tools.map(tool=>({declaration:{name:tool.name,description:tool.description,parameters:tool.parameters,constrainedSampling:tool.constrainedSampling},schema:tool.structuredOutputSchema})),namespaces,budget});}
export function createCodemodeExtension(options){return {name:"codemode",tools:[createCodemodeTool(options),...(options.models===false?[]:createModelTools())],sections:[{key:"codemode",render:input=>codemodeSection(input.agent.callable.filter(tool=>!tool.name.startsWith("pi.codemode.")&&tool.deferred!==true),options.namespaces??[],options.inlineBudget??3000)}]};}
nativeTransport.register("codemode.vm.start",args=>{
 const controller=new AbortController();if(args.aborted)controller.abort(new Error("Aborted"));
 const notify=(event,fields)=>nativeTransport.notify({type:"codemode.vmEvent",run:args.run,event,...fields});
 const start=args.start;
 const run=objectFor(args.sandbox).start(args.code,{startedAt:start.startedAt,seed:start.seed,tools:start.tools.filter(tool=>tool.hidden!==true),globals:start.globals,store:start.store,signal:controller.signal,...(args.timeoutMs==null?{}:{timeoutMs:args.timeoutMs}),onStarted:fingerprint=>notify("started",{fingerprint}),onCall:call=>notify("call",{call}),onIdle:(seq,issued)=>notify("idle",{seq,issued})});
 runs.set(args.run,{run,controller});return null;
});
nativeTransport.register("codemode.vm.result",id=>runs.get(id).run.result);
nativeTransport.register("codemode.vm.finished",id=>runs.get(id)?.run.finished??true);
nativeTransport.register("codemode.vm.deliver",args=>{runs.get(args.run)?.run.deliver(args.seq,args.at,args.call,args.outcome);return null;});
nativeTransport.event("codemode.vm.abort",args=>runs.get(args.run)?.controller.abort(new Error("Code-mode invocation ended")));
nativeTransport.event("codemode.vm.release",args=>runs.delete(args.run));
nativeTransport.register("codemode.global",invokeCodemodeGlobal);
