// Test-only transport. No persistence/recovery logic is implemented in JS.
import {spawn} from "node:child_process";
import {createInterface} from "node:readline";
import {writeFileSync} from "node:fs";
let child,next=1,fsNext=1,exit,closed=false;
const pending=new Map(),filesystems=new Map();
export const nativeStats={opens:0,commits:0,adoptions:0,filesystemCalls:0};
export class JsonlCorruptionError extends Error {constructor(message){super(message);this.name="JsonlCorruptionError";}}
export class JsonlStoragePoisonedError extends Error {constructor(message){super(message);this.name="JsonlStoragePoisonedError";}}
function errorFromNative(value){
 const error=value.name==="JsonlCorruptionError"?new JsonlCorruptionError(value.message):value.name==="JsonlStoragePoisonedError"?new JsonlStoragePoisonedError(value.message):new Error(value.message);
 error.name=value.name;return error;
}
function start(){
 if(child)return;
 if(!process.env.PI_DURABLE_NATIVE_JSONL_BRIDGE)throw new Error("Missing native JSONL bridge binary");
 child=spawn(process.env.PI_DURABLE_NATIVE_JSONL_BRIDGE,[],{stdio:["pipe","pipe","inherit"]});
 exit=new Promise(resolve=>child.once("exit",code=>{for(const wait of pending.values())wait.reject(new Error(`Native JSONL bridge exited ${code}`));pending.clear();resolve(code);}));
 const lines=createInterface({input:child.stdout});
 lines.on("line",line=>{
  const frame=JSON.parse(line);
  if(frame.type==="fs.call"){
   nativeStats.filesystemCalls++;
   const source=filesystems.get(frame.fs);const args=[...frame.args];
   if(frame.method==="appendFile"||frame.method==="writeFile")args[1]=Buffer.from(args[1],"base64");
   const context=pending.get(frame.context)?.context??source.context;
   Promise.resolve().then(()=>source.fs[frame.method](...args,context)).then(result=>{
    const wire=result.ok?{ok:true,value:result.value===undefined?null:frame.method==="readBinaryFile"?Buffer.from(result.value).toString("base64"):result.value}:{ok:false,error:{code:result.error.code,message:result.error.message,path:result.error.path}};
    child.stdin.write(JSON.stringify({type:"fs.result",id:frame.id,result:wire})+"\n");
   },error=>{child.stdin.write(JSON.stringify({type:"fs.result",id:frame.id,result:{ok:false,error:{code:"unknown",message:error.message}}})+"\n");});
   return;
  }
  const wait=pending.get(frame.id);if(!wait)throw new Error(`Unknown native response ${frame.id}`);
  if(frame.type==="adopt"){nativeStats.adoptions++;try{wait.adopt?.(frame.seq);}catch(error){wait.adoptionError=error;}return;}
  pending.delete(frame.id);
  if(frame.error)wait.reject(errorFromNative(frame.error));else if(wait.adoptionError)wait.reject(wait.adoptionError);else wait.resolve(frame.missing?undefined:frame.value);
 });
}
function call(request,context,adopt){
 start();
 return new Promise((resolve,reject)=>{
  const id=next++;
  // Serialization and detachment happen before returning the commit promise.
  const line=JSON.stringify({...request,id,context:id})+"\n";
  pending.set(id,{resolve,reject,context,adopt});child.stdin.write(line);
 });
}
const arities={mintId:0,conversation:1,scanConversations:3,findLatestHeadMarker:2,scanEntries:3,task:1,scanTasks:3,submission:1,scanSubmissions:3,submissionByRequest:2,findDocument:2,document:2,scanDocuments:3,close:0};
export class JsonlStorage {
 constructor(handle){this.handle=handle;}
 static async open(directory,fs,context,options={}){
  const fsId=fsNext++;filesystems.set(fsId,{fs,context});nativeStats.opens++;
  const handle=await call({method:"open",directory,fs:fsId,cwd:fs.cwd,fsNamespace:fs.id,options},context);
  return new JsonlStorage(handle);
 }
 commit(writes,adopt,context){nativeStats.commits++;return call({handle:this.handle,method:"commit",args:[writes]},context,adopt);}
 entry(...args){return call({handle:this.handle,method:"entry",args:args.slice(0,-1)},args.at(-1));}
}
for(const [method,arity] of Object.entries(arities))JsonlStorage.prototype[method]=function(...args){return call({handle:this.handle,method,args:args.slice(0,arity)},args[arity]);};
export async function shutdown(){
 if(closed)return;closed=true;
 if(!child)throw new Error("Stock tests never reached the native JSONL backend");
 if(pending.size)throw new Error(`Native requests still pending: ${pending.size}`);
 child.stdin.end();const code=await exit;if(code!==0)throw new Error(`Native JSONL bridge failed: ${code}`);
 if(nativeStats.opens===0||nativeStats.commits===0||nativeStats.filesystemCalls===0)throw new Error("Stock tests did not exercise native persistence");
 if(process.env.PI_DURABLE_NATIVE_JSONL_STATS)writeFileSync(process.env.PI_DURABLE_NATIVE_JSONL_STATS,JSON.stringify(nativeStats));
 console.log(`NATIVE_JSONL_STOCK ${JSON.stringify(nativeStats)}`);
}
