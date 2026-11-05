// Synchronous leaf binding for the unchanged 128 MB subprocess test.
import {execFileSync} from "node:child_process";
export function toBase64(bytes){return execFileSync(process.env.PI_DURABLE_NATIVE_TASK_BRIDGE,["--stock-base64"],{input:Buffer.from(bytes),encoding:"utf8",maxBuffer:16*1024*1024});}
