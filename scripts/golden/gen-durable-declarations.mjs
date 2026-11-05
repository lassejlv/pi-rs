// Independent UTF-16 expectations from the pinned declaration renderer.
import assert from "node:assert/strict";
import {execFileSync} from "node:child_process";
import {readFile,writeFile} from "node:fs/promises";
import {resolve,join} from "node:path";
import {fileURLToPath,pathToFileURL} from "node:url";
import {createHash} from "node:crypto";
const root=fileURLToPath(new URL("../../",import.meta.url)),reference=resolve(process.argv[2]??"../pi-mono"),pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);
const sources=["packages/codemode/src/declarations.ts","packages/codemode/src/identifier.ts"],hashes={};
for(const source of sources){const bytes=await readFile(join(reference,source));assert.deepEqual(bytes,execFileSync("git",["show",`${pin}:${source}`],{cwd:reference}));hashes[source]=createHash("sha256").update(bytes).digest("hex");}
const {renderDeclarations,renderToolSample,renderToolSignature,schemaToType}=await import(pathToFileURL(join(reference,sources[0])));
const raw=String.fromCharCode(0xd800),low=String.fromCharCode(0xdc01),pair='\u{1f600}';
const schemas=[true,false,{},null,[],{type:'string'},{const:raw},{enum:[raw,low,pair,raw,null,2]},
 {type:'object',properties:{[raw]:{type:'string',description:` ${raw}\n${low}\u2028 ${pair} `},[low]:{const:raw},[pair]:{type:'integer',minimum:2},'\ue000':{type:'array',items:{type:'string'}}},required:[raw,pair],additionalProperties:false},
 {$defs:{[raw]:{type:'object',properties:{[low]:{const:raw}},required:[low]}},$ref:`#/$defs/${raw}`},
 {$defs:{'a/b~c':{const:low}},$ref:'#/$defs/a~1b~0c'},
 {$defs:{'é':{const:raw}},$ref:'#/$defs/%C3%A9'},
 {type:'object',properties:{x:{type:'string',description:` a\r\n${raw}\u2029${low} `,pattern:`${raw}\u2028${low}`,default:raw},y:{anyOf:[{type:'integer',minimum:10,format:low},{type:'null'}],minimum:2,maximum:20,format:raw}}},
 {type:'object',properties:{x:{type:'string',pattern:raw.repeat(71)},y:{type:'string',pattern:raw.repeat(72)},z:{type:'string',default:{[low]:raw}}}},
 {type:'object',properties:{x:{type:'number',minimum:0,exclusiveMinimum:true,maximum:10,exclusiveMaximum:false,deprecated:true}}},
 {allOf:[{anyOf:[{const:raw},{const:low}]},{type:'object',properties:{[raw]:{type:'boolean'}}}]},
 {type:'array',prefixItems:[{const:raw},{type:['string','null']}]},
 {type:['string','object','array'],properties:{[raw]:{type:'string'}},items:{const:low}},
 {$ref:'#'},{$defs:{x:{$ref:'#/$defs/x'}},$ref:'#/$defs/x'},
 {type:'object',properties:{nested:{type:'object',description:raw,properties:{[raw]:{type:'string',description:low}}}},additionalProperties:true}
];
const rows=schemas.map((schema,i)=>{
 const tool={name:`${raw}tool.${pair}-${i}`,description:`\ufeff ${raw} */\r\n ${low}\n`,inputSchema:schema,outputSchema:{const:low}};
 const globals=[{name:`${raw}global`,description:low,signature:`(arg: '${raw}'): void`},{name:`${raw}ns.${low}member`,description:`${raw}\n${low}`,inputSchema:schema,outputSchema:{const:raw}}];
 return {schema,tool,globals,types:[undefined,0,10,40,80,16000].map(maxChars=>({maxChars:maxChars??null,expected:schemaToType(schema,{maxChars})})),signature:renderToolSignature(tool),sample:renderToolSample(tool),declarations:renderDeclarations({tools:[tool],globals})};
});
const output=join(root,"crates/pi-codemode/tests/fixtures/declarations-reference.json"),contents=JSON.stringify({reference:pin,hashes,rows},null,2)+"\n";
if(process.argv.includes("--check"))assert.equal(await readFile(output,"utf8"),contents,"Declaration fixture is stale");else await writeFile(output,contents);
for(const source of sources)assert.equal(createHash("sha256").update(await readFile(join(reference,source))).digest("hex"),hashes[source]);
console.log(`${rows.length} independent schemas; ${rows.reduce((n,r)=>n+r.types.length+3,0)} rendering comparisons`);
