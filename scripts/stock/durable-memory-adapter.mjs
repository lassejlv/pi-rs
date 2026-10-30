// Plain memory stores share the native backend directly with the Session.
// Subclasses retain the source implementations and all their original controls.
import {nativeTransport} from "./durable-native-transport.mjs";
import {handle,objectFor} from "./durable-native-handles.mjs";
const stores=new WeakMap();
const methods=["commit","mintId","conversation","scanConversations","entry","findLatestHeadMarker","scanEntries","task","scanTasks","submission","scanSubmissions","submissionByRequest","findDocument","document","scanDocuments","close"];
export function createNativeMemoryAdapter(ReferenceMemoryStorage){
 class MemoryStorage extends ReferenceMemoryStorage {
  constructor(...args){super(...args);if(new.target===MemoryStorage)stores.set(this,{id:nativeTransport.sync("memory.new"),constructor:MemoryStorage});}
 }
 for(const method of methods)Object.defineProperty(MemoryStorage.prototype,method,{configurable:true,writable:true,value:function(...args){
  const held=stores.get(this);
  if(held===undefined)return Reflect.apply(ReferenceMemoryStorage.prototype[method],this,args);
  const context=args.pop();let adopt;
  if(method==="commit")adopt=handle(args.splice(1,1)[0]);
  return nativeTransport.request("memory.call",{memory:held.id,operation:method,args,adopt},context);
 }});
 return MemoryStorage;
}
export function nativeMemoryHandle(storage){
 const held=stores.get(storage);
 return held!==undefined&&Object.getPrototypeOf(storage)===held.constructor.prototype&&methods.every(method=>!Object.hasOwn(storage,method))?held.id:undefined;
}
nativeTransport.register("memory.adopt",args=>{objectFor(args.callback)(args.seq);return null;});
