// Opaque capabilities carry original object identity across the test transport.
import {nativeTransport} from "./durable-native-transport.mjs";
let next=1;
const objects=new Map(),ids=new WeakMap(),nativeTools=new WeakSet(),nativeGenerations=new WeakSet(),nativeCompactions=new WeakSet();
export function markNativeCompaction(task){nativeCompactions.add(task);}
export function markNativeGeneration(task){nativeGenerations.add(task);}
export function markNativeTool(task){nativeTools.add(task);}
export function handle(value){let id=ids.get(value);if(id===undefined){id=next++;ids.set(value,id);objects.set(id,value);}return id;}
export function valueHandle(value){if((typeof value==="object"&&value!==null)||typeof value==="function")return handle(value);const id=next++;objects.set(id,value);return id;}
export function objectFor(id){return objects.get(id);}
export function temporaryHandle(value){const id=next++;objects.set(id,value);return {id,release:()=>objects.delete(id)};}
export function describeTask(task){const {name,version,phases,migrate}=task.definition;return{id:handle(task),name,version,phases:Object.keys(phases),migrate:migrate!==undefined,...(nativeTools.has(task)?{nativeTool:true}:{}),...(nativeGenerations.has(task)?{nativeGeneration:true}:{}),...(nativeCompactions.has(task)?{nativeCompaction:true}:{})};}
export function describeDoc(token){const {kind,version,scope,history,fork,family,migrate}=token.definition;return {id:handle(token),kind,version,scope,history,fork,family,migrate:migrate!==undefined};}
nativeTransport.register("initial",([id,input])=>objectFor(id).definition.initial(input));
nativeTransport.register("migrate",([id,input,checkpoint,version])=>objectFor(id).definition.migrate(input,checkpoint,version));
