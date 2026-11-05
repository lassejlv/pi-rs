// This worker only moves frames. The test thread can synchronously pump host
// callbacks, including callbacks that make nested requests to the native host.
import {spawn} from "node:child_process";
import {createInterface} from "node:readline";
import {workerData} from "node:worker_threads";
const {port,signal,binary}=workerData;
const state=new Int32Array(signal);
function deliver(frame){port.postMessage(frame);Atomics.add(state,0,1);Atomics.notify(state,0);}
const child=spawn(binary,[],{stdio:["pipe","pipe","inherit"]});
let failed=false;
child.once("error",error=>{failed=true;deliver({type:"transport.error",message:error.message});});
child.once("exit",code=>{if(!failed)deliver({type:"transport.exit",code});port.close();});
createInterface({input:child.stdout}).on("line",line=>{
 try{deliver(JSON.parse(line));}catch(error){deliver({type:"transport.error",message:error.message});}
});
port.on("message",frame=>{if(frame.type==="transport.close")child.stdin.end();else child.stdin.write(JSON.stringify(frame)+"\n");});
