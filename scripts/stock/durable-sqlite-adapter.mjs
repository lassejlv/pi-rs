// Test-only transport. SQLite schemas, migrations and Storage methods run in Rust.
import {writeFileSync} from "node:fs";
import {nativeTransport as transport} from "./durable-native-transport.mjs";
const databases=new Map(),transactions=new Map(),adoptions=new Map();
let databaseNext=1;
export const nativeStats={opens:0,commits:0,adoptions:0,hostCalls:0,hostTransactions:0,admitted:0};
const errorWire=error=>({name:error.name,message:error.message});
function host(database){const id=databaseNext++;databases.set(id,database);return id;}
transport.event("database.call",frame=>{
 nativeStats.hostCalls++;
 const reply=value=>transport.notify({type:"database.result",id:frame.id,value:value??null});
 const reject=error=>transport.notify({type:"database.result",id:frame.id,error:errorWire(error)});
 try{
  const database=databases.get(frame.handle),params=frame.params.map(p=>p&&typeof p==="object"&&p.blob?new Uint8Array(p.blob):p);
  if(!database)throw new Error(`Missing SQLite host ${frame.handle}`);
  const result=frame.method==="close"?database.close():database[frame.method](frame.sql,...params);
  Promise.resolve(result).then(reply,reject);
 }catch(error){reject(error);}
});
transport.event("database.transaction",frame=>{
 nativeStats.hostTransactions++;
 try{
  const result=databases.get(frame.handle).transaction(executor=>new Promise((resolve,reject)=>{
   const handle=host(executor);transactions.set(frame.id,{resolve,reject,handle});
   transport.notify({type:"transaction.begin",id:frame.id,handle});
  }));
  Promise.resolve(result).then(()=>transport.notify({type:"database.result",id:frame.id,value:null}),error=>transport.notify({type:"database.result",id:frame.id,error:errorWire(error)}));
 }catch(error){transport.notify({type:"database.result",id:frame.id,error:errorWire(error)});}
});
transport.event("transaction.result",frame=>{
 const waiting=transactions.get(frame.id);transactions.delete(frame.id);databases.delete(waiting.handle);
 frame.error?waiting.reject(new Error(frame.error.message)):waiting.resolve();
});
transport.event("adopt",frame=>{
 nativeStats.adoptions++;
 const waiting=adoptions.get(frame.id);
 try{waiting.adopt(frame.seq);}catch(error){waiting.error=error;}
});
function call(method,args={},adopt){
 const id=transport.nextId,waiting={adopt};
 if(adopt)adoptions.set(id,waiting);
 let promise;
 try{promise=transport.requestAdmitted(method,args);nativeStats.admitted++;}
 catch(error){adoptions.delete(id);return Promise.reject(error);}
 const result=promise.then(value=>{if(waiting.error)throw waiting.error;return value;}).finally(()=>adoptions.delete(id));
 result.catch(()=>{});return result;
}
const configuration=transport.sync("configuration");
export const SQLITE_MIGRATIONS=configuration.migrations;
export const CURRENT_SQLITE_SCHEMA_VERSION=configuration.version;
export const DEFAULT_SQLITE_SCHEMA=configuration.schema;
export const applySqliteMigrations=(database,migrations)=>call("migrate",{database:host(database),migrations});
const arities={mintId:0,conversation:1,scanConversations:3,findLatestHeadMarker:2,scanEntries:3,task:1,scanTasks:3,submission:1,scanSubmissions:3,submissionByRequest:2,findDocument:2,document:2,scanDocuments:3};
export class SqliteStorage {
 constructor(handle){this.handle=handle;}
 static open(database,options={}){nativeStats.opens++;return call("open",{database:host(database),options}).then(handle=>new SqliteStorage(handle));}
 commit(writes,adopt){
  nativeStats.commits++;
  try{return call("commit",{handle:this.handle,args:[JSON.parse(JSON.stringify(writes))]},adopt);}
  catch(error){return Promise.reject(error);}
 }
 entry(...args){return call("entry",{handle:this.handle,args:args.slice(0,-1)});}
 close(){return this.closing??=call("close",{handle:this.handle,args:[]});}
}
for(const [method,arity] of Object.entries(arities))SqliteStorage.prototype[method]=function(...args){return call(method,{handle:this.handle,args:args.slice(0,arity)});};
export function openNodeSqliteStorage(path,options={}){
 nativeStats.opens++;return call("openNative",{path,options}).then(handle=>new SqliteStorage(handle));
}
export async function shutdown(){
 if(transactions.size||adoptions.size)throw new Error("Unsettled SQLite callbacks");
 await transport.close();
 if(nativeStats.opens===0||nativeStats.commits===0||nativeStats.admitted===0)throw new Error("Stock tests bypassed native SQLite storage");
 if(process.env.PI_DURABLE_NATIVE_SQLITE_STATS)writeFileSync(process.env.PI_DURABLE_NATIVE_SQLITE_STATS,JSON.stringify(nativeStats));
 console.log(`NATIVE_SQLITE_STOCK ${JSON.stringify(nativeStats)}`);
}
