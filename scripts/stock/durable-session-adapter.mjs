// Declared independently so reference class definitions can extend the binding
// without importing the original Session kernel or entering a package cycle.
let bind,defaultRetention=Number(process.env.PI_DURABLE_DOCUMENT_RETENTION_MS??600_000),defaultTimer;
function operation(start){let result;try{result=Promise.resolve(start());}catch(error){result=Promise.reject(error);}result.catch(()=>{});return result;}
export function setDefaultDocumentRetention(value){defaultRetention=value;}
export function getDefaultDocumentRetention(){return defaultRetention;}
export function setDefaultSessionTimer(timer){defaultTimer=timer;}
export function installSessionBinding(binding){bind=binding;}
export class SessionImpl {
 constructor(storage,now=Date.now,retention=()=>undefined,setTimer=defaultTimer,readOnly=false){
  if(bind===undefined)throw new Error("Native Session transport was not installed");
  this.binding=bind(this,storage,now,()=>retention()??defaultRetention,setTimer,readOnly);
 }
 commit(callback,context){return this.commitWith(callback,context);}
 commitWith(callback,context,scope){return operation(()=>this.binding.commitWith(callback,context,scope));}
 snapshot(...args){return operation(()=>this.binding.snapshot(...args));}
 snapshotAsOf(...args){return operation(()=>this.binding.snapshotAsOf(...args));}
 watchDoc(...args){return operation(()=>this.binding.watchDoc(...args));}
 readOnLine(job){return operation(()=>this.binding.readOnLine(job));}
 subscribeClose(callback){return this.binding.subscribeClose(callback);}
 subscribeCommits(callback){return this.binding.subscribeCommits(callback);}
 get closed(){return this.binding.closed();}
 close(context){return operation(()=>this.binding.close(context));}
 unloadDocuments(){return operation(()=>this.binding.unloadDocuments());}
 get documentCacheSizes(){return this.binding.cache();}
 async conversationCreated(){}
 async beforeClose(){}
 report(){}
}
export function createSession(storage,options){return new SessionImpl(storage,options?.now,options?.documentRetentionMs===undefined?undefined:()=>options.documentRetentionMs,options?.setTimer);}
