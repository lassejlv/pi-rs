// The stock suite supplies callbacks and its outer Harness. Rust owns prompt decisions.
import {nativeTransport,failure} from "./durable-native-transport.mjs";
import {handle,objectFor} from "./durable-native-handles.mjs";
import {shutdownContextTransport} from "./durable-task-adapter.mjs";
nativeTransport.register("promptRender",({section,input},context)=>objectFor(section).render(objectFor(input),context));
nativeTransport.register("promptReport",({report,section,error})=>{objectFor(report)(failure(error),objectFor(section));return null;});
export function replaySections(messages){return new Map(nativeTransport.sync("prompt.replay",{messages}));}
export function planSystemEntries(view,desired,tools,timestamp){
 const declarations=tools.map(({name,description,parameters,constrainedSampling})=>({name,description,parameters:JSON.parse(JSON.stringify(parameters)),...(constrainedSampling===undefined?{}:{constrainedSampling})}));
 return nativeTransport.sync("prompt.plan",{view:{head:view.head??null,entries:view.entries,messages:view.messages},desired:[...desired],tools:declarations,timestamp});
}
export async function renderSections(sections,input,shown,report,context){
 return new Map(await nativeTransport.request("prompt.render",{sections:sections.map(section=>({id:handle(section),key:section.key,tagged:section.tag!==false})),input:handle(input),shown:[...shown],report:handle(report)},context));
}
export async function shutdownPrompt(){
 const stats=await nativeTransport.request("stats");
 for(const key of ["promptReplays","promptPlans","promptRenders","promptSectionCalls"])if(!(stats[key]>0))throw new Error(`Native prompt was bypassed: ${key}`);
 await shutdownContextTransport();
}
