import {execFileSync} from "node:child_process";
import {readFile,writeFile} from "node:fs/promises";
import {resolve} from "node:path";
import {pathToFileURL} from "node:url";
const pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
const reference=resolve(process.argv[2]??"../pi-mono");
if(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim()!==pin) throw new Error("Reference differs from pin");
const o=await import(pathToFileURL(resolve(reference,"packages/chord/src/output.ts")));
const texts=["","abc","a\nb\nc\n","a\nb\nc","\n\n\n","long line followed\nshort\n","short\nlong line following","a\u{1d11e}b\nc\u00e9d\n","\ufeffa\n\ufeffb","a\u0000b\u0008c\r\t\n\ufff9\ufffb","a\ud800b\n\udfff"];
const limits=[];
for(const retain of ["head","tail"]) for(const [maxBytes,maxLines] of [[0,0],[0,2],[2,0],[1,1],[2,1],[3,2],[7,2],[100,10]]) limits.push({maxBytes,maxLines,retain});
const bounded=[];
for(const text of texts) for(const limit of limits) bounded.push({text,limits:limit,result:o.boundOutput(text,limit),sanitized:o.sanitizeOutput(text),byteLength:o.utf8ByteLength(text)});
const streams=[];
function stream(limits,chunks){
 const buffer=new o.OutputBuffer(limits);
 const steps=chunks.map(chunk=>{
   const input=chunk.kind==="bytes"?new Uint8Array(chunk.data):chunk.data;
   const accepted=buffer.push(input,chunk.skipped);
   const result=buffer.snapshot();
   return {...chunk,accepted,result,storedBytes:buffer.storedBytes};
 });
 buffer.end();
 streams.push({limits,steps,result:buffer.snapshot(),storedBytes:buffer.storedBytes});
}
for(const limit of limits) {
 for(const text of texts){
   stream(limit,Array.from(text).map(data=>({kind:"text",data})));
   const bytes=new TextEncoder().encode(text);
   for(const size of [1,3,7]) {
     const chunks=[];for(let i=0;i<bytes.length;i+=size) chunks.push({kind:"bytes",data:Array.from(bytes.slice(i,i+size))});
     stream(limit,chunks);
   }
 }
 stream(limit,[{kind:"bytes",data:[0xf0,0x9d]},{kind:"text",data:"next\n"},{kind:"bytes",data:[0x84,0x9e,0xff,0xc0,0xaf]}]);
 stream(limit,[{kind:"bytes",data:[0xef]},{kind:"bytes",data:[0xbb,0xbf,0x61]},{kind:"bytes",data:[0xef,0xbb,0xbf,0x62]}]);
 if(limit.retain==="tail") stream(limit,[{kind:"text",data:"old\n"},{kind:"bytes",data:[0xe2]},{kind:"text",data:"new\n",skipped:{bytes:1000,newlines:100,endsWithNewline:true}},{kind:"text",data:"tail"}]);
}
const target=new URL("../../crates/pi-chord/tests/fixtures/output.json",import.meta.url);
const output=JSON.stringify({reference:pin,bounded,streams},null,2)+"\n";
if(process.argv.includes("--check")){if(await readFile(target,"utf8")!==output)throw new Error("Output fixtures differ");}
else await writeFile(target,output);
console.log(`Verified ${bounded.length} output bounds and ${streams.length} chunked streams`);
