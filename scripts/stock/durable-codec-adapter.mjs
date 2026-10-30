// Synchronous stock bindings; decoding and scanning execute the production Rust.
import {readFileSync} from "node:fs";
const path=process.env.PI_DURABLE_CODEC_WASM;
if(!path)throw new Error("Missing native durable codec WebAssembly module");
const wasm=new WebAssembly.Instance(new WebAssembly.Module(readFileSync(path))).exports;
const decoder=new TextDecoder("utf-8",{ignoreBOM:true});
let decodes=0,scans=0;
function input(bytes){const at=wasm.input_buffer(bytes.length);new Uint8Array(wasm.memory.buffer,at,bytes.length).set(bytes);}
function output(length){return decoder.decode(new Uint8Array(wasm.memory.buffer,wasm.output_buffer(),length));}
export class StreamDecoder {
 constructor(){this.id=wasm.decoder_create(0);}
 decode(bytes){input(bytes??new Uint8Array());decodes++;return output(wasm.decoder_decode(this.id,bytes===undefined?1:0));}
}
export function rangeDecoder(){
 const id=wasm.decoder_create(1);
 return {decode(bytes,options){input(bytes??new Uint8Array());decodes++;return output(wasm.decoder_decode(id,options?.stream?0:1));}};
}
export function startsWithBom(bytes){input(bytes);return wasm.starts_bom()!==0;}
export class LineScanner {
 constructor(start,end=Infinity){this.id=wasm.scanner_create(start,end);if(this.id<0)throw new RangeError("Invalid line range");}
 push(bytes){input(bytes);wasm.scanner_push(this.id);}
 finish(){scans++;return JSON.parse(output(wasm.scanner_finish(this.id)));}
}
export function counters(){return {decodes,scans};}
