// Independent expected results from the pinned, unchanged TypeScript module.
import assert from "node:assert/strict";
import {execFileSync} from "node:child_process";
import {readFile,writeFile,mkdir} from "node:fs/promises";
import {resolve,join,dirname} from "node:path";
import {fileURLToPath,pathToFileURL} from "node:url";
import {createHash} from "node:crypto";
const root=fileURLToPath(new URL("../../",import.meta.url)),reference=resolve(process.argv[2]??"../pi-mono"),pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);
const sources=["edit-diff.ts","edit.ts","write.ts"].map(name=>`packages/durable/src/tools/${name}`),hashes={};
for(const path of sources){const bytes=await readFile(join(reference,path));assert.deepEqual(bytes,execFileSync("git",["show",`${pin}:${path}`],{cwd:reference}));hashes[path]=createHash("sha256").update(bytes).digest("hex");}
const source=await import(pathToFileURL(join(reference,sources[0])));
const {createWriteTool}=await import(pathToFileURL(join(reference,sources[2])));
const {createEditTool}=await import(pathToFileURL(join(reference,sources[1])));
const cases=[];
function add(operation,args){let expected;try{switch(operation){case"apply":expected=source.applyEditsToNormalizedContent(args.content,args.edits,args.path);break;case"diff":expected=source.generateDiffString(args.old,args.new,args.context);break;case"patch":expected=source.generateUnifiedPatch(args.path,args.old,args.new,args.context);break;case"fuzzy":expected=source.fuzzyFindText(args.content,args.oldText);break;case"normalize":expected=source.normalizeForFuzzyMatch(args.text);break;}}catch(error){expected={error:error.message};}cases.push({operation,args,expected});}
const pieces=["", "a", "b", " ", "\t", "\n", "\r\n", "\u2019", "\u2014", "\u00a0", "\u2007", "\u2000", "\uFEFF", "\uFB03", "\uFF21", "e\u0301", "\uD834\uDD1E", "\uD800", "\uDC00"];
let seed=0x12345678;const next=()=>{seed=(Math.imul(seed,1664525)+1013904223)>>>0;return seed;};
function text(){return Array.from({length:next()%35},()=>pieces[next()%pieces.length]).join("");}
for(let i=0;i<400;i++){
 const content=text(),replacement=text(),path=`f-${i}-\uD800.txt`,needle=i%3===0?content.slice(next()%(content.length+1)):text();
 add("normalize",{text:content});add("fuzzy",{content,oldText:needle});
 add("apply",{content:source.normalizeToLF(content),edits:[{oldText:needle,newText:replacement},...(i%4===0?[{oldText:content.slice(0,next()%(content.length+1)),newText:"changed"}]:[])],path});
 add("diff",{old:content,new:replacement,context:[0,1,4,8][i%4]});add("patch",{path,old:content,new:replacement,context:[0,1,4,8][i%4]});
}
for(const [content,edits] of [
 ["keep\u2019  \nalpha\u2014  \nkeep\u2019  \n",[{oldText:"alpha-",newText:"ALPHA"}]],
 ["x\nx\na\nx\nx\n",[{oldText:"a",newText:"a\nb"}]],
 ["\uFB03 one\n\uFB03 two\n",[{oldText:"ffi one",newText:"first"},{oldText:"ffi two",newText:"second"}]],
 ["a\nb\nc\n",[{oldText:"a\nb",newText:"A"},{oldText:"b\nc",newText:"B"}]],
 ["a\nb\nc\n",[{oldText:"b",newText:"b"}]],
]) add("apply",{content,edits,path:"f"});
const lines=Array.from({length:35},(_,i)=>`line-${i}`).join("\n");for(const context of [0,1,4,8]){const changed=lines.replace("line-1\n","changed-1\n").replace("line-18\n","changed-18\n");add("diff",{old:lines,new:changed,context});add("patch",{path:"long",old:lines,new:changed,context});}
const declarations=[createWriteTool(),createEditTool()].map(({name,description,parameters})=>({name,description,parameters}));
const output=JSON.stringify({reference:pin,hashes,declarations,cases},null,2)+"\n",destination=join(root,"crates/pi-durable/tests/fixtures/edit-reference.json");
if(process.argv.includes("--check"))assert.equal(await readFile(destination,"utf8"),output);else{await mkdir(dirname(destination),{recursive:true});await writeFile(destination,output);}
console.log(`Reference coding mutations: ${cases.length} independent normalization, UTF16 matching, replacement, display-diff and patch cases; two tool declarations`);
