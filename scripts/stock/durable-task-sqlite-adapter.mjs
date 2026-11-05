// Test-only bindings to the same native SQLite storage used by the Rust Session.
// WeakMap identities keep proxy gates and decorated methods on their host path.
import {nativeTransport} from "./durable-native-transport.mjs";
import {temporaryHandle} from "./durable-native-handles.mjs";
const stores=new WeakMap();
const methods=["commit","mintId","conversation","scanConversations","entry","findLatestHeadMarker","scanEntries","task","scanTasks","submission","scanSubmissions","submissionByRequest","findDocument","document","scanDocuments","close"];
class NativeSqliteStorage {}
for(const operation of methods)Object.defineProperty(NativeSqliteStorage.prototype,operation,{configurable:true,writable:true,value:function(...args){
 const store=stores.get(this),context=args.pop();
 if(operation==="close"&&store.closing)return store.closing;
 const adopt=operation==="commit"?temporaryHandle(args.splice(1,1)[0]):undefined;
 const result=nativeTransport.request("sqlite.call",{sqlite:store.id,operation,args,adopt:adopt?.id},context);
 const response=adopt?result.finally(adopt.release):result;response.catch(()=>{});
 if(operation==="close")store.closing=response;
 return response;
}});
export async function openNodeSqliteStorage(path,options={}){
 const id=await nativeTransport.request("sqlite.new",{path,options});
 const target=new NativeSqliteStorage();let decorated=false;
 const changed=property=>{if(!decorated&&methods.includes(property)){decorated=true;nativeTransport.sync("sqlite.decorate",{sqlite:id});}};
 const storage=new Proxy(target,{
  set(target,property,value){const result=Reflect.set(target,property,value);if(result)changed(property);return result;},
  defineProperty(target,property,descriptor){const result=Reflect.defineProperty(target,property,descriptor);if(result)changed(property);return result;},
  deleteProperty(target,property){const result=Reflect.deleteProperty(target,property);if(result)changed(property);return result;},
 });
 stores.set(storage,{id});return storage;
}
export function nativeSqliteHandle(storage){
 const store=stores.get(storage);
 return store!==undefined&&Object.getPrototypeOf(storage)===NativeSqliteStorage.prototype&&methods.every(method=>!Object.hasOwn(storage,method))?store.id:undefined;
}
