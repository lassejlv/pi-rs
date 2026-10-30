// Independent formatting expectations from the pinned source, including UTF16 cuts.
import assert from "node:assert/strict";
import {execFileSync} from "node:child_process";
import {readFile,writeFile} from "node:fs/promises";
import {resolve,join} from "node:path";
import {fileURLToPath,pathToFileURL} from "node:url";
import {createHash} from "node:crypto";
const root=fileURLToPath(new URL("../../",import.meta.url)),reference=resolve(process.argv[2]??"../pi-mono"),pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);
const source="packages/durable/src/codemode/output.ts",bytes=await readFile(join(reference,source));
assert.deepEqual(bytes,execFileSync("git",["show",`${pin}:${source}`],{cwd:reference}));
const {formatResult}=await import(pathToFileURL(join(reference,source)));
const image={type:"image",data:"YQ",mimeType:"image/png"},text=value=>({type:"text",text:value});
const outputs=[[],[text("")],[text("one"),text(""),text("two")],[text("one\n"),image,text("two"),image],[{...text("one"),console:true},text("two"),{...text("three"),console:true}],[text("a\ud800b\udc00c\ud83d\ude00\n".repeat(19))],[image,{...image,mimeType:"image/jpeg"}]];
const cases=[];
for(let i=0;i<112;i++){
 const result={ok:i%4!==0,output:outputs[i%outputs.length]};
 if(result.ok){if(i%3===0)result.value=i%2===0?{["\ud800"]:"value",2:2,1:1}:null;else if(i%3===1)result.value="return\ud800";}
 else result.error={kind:["script","timeout","aborted","sandbox"][Math.floor(i/4)%4],name:"Custom",message:"failure\ud800",...(i%8===0?{stack:"stored stack"}:{})};
 const options={generatedImages:i%5===0?2:0,wallMs:[0,1250,1249,10000][i%4],maxOutputTokens:[0,1,10,10000][i%4],calls:i%2?[{name:"tool\udc00",status:"error"}]:[],limits:{maxBytes:i%7===0?120:4194304,maxLines:i%9===0?110:100000}};
 const writes=[],fail=i%6===0;
 const expected=await formatResult(result,{...options,write:async(name,data)=>{writes.push({name,...(typeof data==="string"?{text:data}:{bytes:[...data]})});return fail?{error:"write failed\ud800"}:{path:`/output/${name}`};}});
 cases.push({result,options,fail,writes,expected});
}
const output=join(root,"crates/pi-durable/tests/fixtures/codemode-output-reference.json"),contents=JSON.stringify({reference:pin,sha256:createHash("sha256").update(bytes).digest("hex"),cases},null,2)+"\n";
if(process.argv.includes("--check"))assert.equal(await readFile(output,"utf8"),contents,"Code-mode output fixture is stale");else await writeFile(output,contents);
assert.deepEqual(await readFile(join(reference,source)),bytes);
console.log(`Independent code-mode output cases: ${cases.length}`);
