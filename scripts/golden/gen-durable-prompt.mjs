// Independent planner/replay expectations from the pinned source, without a native adapter.
import assert from "node:assert/strict";
import {execFileSync} from "node:child_process";
import {readFile,writeFile} from "node:fs/promises";
import {resolve} from "node:path";
import {pathToFileURL,fileURLToPath} from "node:url";
import {registerReferenceSource} from "./reference-source.mjs";
const reference=resolve(process.argv[2]??"../pi-mono"),pin="5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
assert.equal(execFileSync("git",["rev-parse","HEAD"],{cwd:reference,encoding:"utf8"}).trim(),pin);
registerReferenceSource(reference);
const load=path=>import(pathToFileURL(resolve(reference,path)));
const {planSystemEntries,replaySections}=await load("packages/durable/src/harness/prompt.ts");
const {getCurrentTools,toToolDeclaration}=await load("packages/ai/src/utils/transcript.ts");
const system=(sections,toolsAdded,toolsRemoved)=>({role:"system",content:"",timestamp:1,...(sections===undefined?{}:{sections}),...(toolsAdded===undefined?{}:{toolsAdded}),...(toolsRemoved===undefined?{}:{toolsRemoved})});
const tool=(name,description=name,parameters={type:"object",properties:{}})=>({name,description,parameters});
const entry=(id,kind="pi.system")=>({id,conversationId:1,kind});
const cases=[];
function add(name,messages,desired,tools=[],head=null,entries=[]){
 const view={...(head===null?{}:{head}),entries,contributions:[],messages};
 const drafts=planSystemEntries(view,new Map(desired),tools,7.5);
 const next=[...messages,...drafts.flatMap(draft=>draft.model??[])];
 cases.push({name,view,desired,tools:tools.map(toToolDeclaration),drafts,shown:[...replaySections(messages)],replayed:[...replaySections(next)],offered:getCurrentTools(next)});
 return drafts;
}
const a=tool("a"),b=tool("b"),c=tool("c");
for(const shown of [[],[["a","1"]],[["a","1"],["b","2"]],[["b","2"],["a","1"]]]){
 for(const desired of [[],[["a","1"]],[["a","changed"]],[["a","1"],["b","2"]],[["b","2"],["a","1"]],[["b","2"],["c","3"],["a","1"]]]){
  for(const tools of [[],[a],[a,b],[b,a],[a,tool("b","changed")]]){
   add(`section/tool matrix ${cases.length}`,[system(Object.fromEntries(shown),[a,b])],desired,tools);
  }
 }
}
add("replay ignores non-system patches and re-adds at end",[system({a:"1",b:"2",c:"3"},[a,b]),{role:"user",sections:{ignored:"x"},toolsAdded:[c]},system({b:"20",a:null},[tool("a","new")],[{name:"b"}]),system({a:"10"},[b])],[["b","20"],["c","3"],["a","10"]],[b,tool("a","new")]);
add("schema property order changes serialized declaration",[system(undefined,[tool("a","a",{type:"object",properties:{x:{type:"string"},y:{type:"number"}}})])],[],[tool("a","a",{type:"object",properties:{y:{type:"number"},x:{type:"string"}}})]);
add("constrained false differs from absence",[system(undefined,[a])],[],[{...a,constrainedSampling:false}]);
add("constrained null survives",[system(undefined,[{...a,constrainedSampling:false}])],[],[{...a,constrainedSampling:null}]);
add("declaration fields ignore host metadata",[system(undefined,[a])],[],[{...a,execute:()=>{},title:"host metadata",parameters:{...a.parameters,ignored:undefined}}]);
add("duplicate declarations keep map positions",[system(undefined,[a,b,tool("a","new")]),system(undefined,[a],[{name:"b"}])],[],[a,b]);
add("duplicate desired names preserve source decisions",[system(undefined,[a])],[],[a,tool("a","new")]);
add("numeric section keys enumerate before strings",[system(JSON.parse('{"z":"z","10":"ten","2":"two","01":"one","4294967295":"max"}'))],[["10","changed"],["2","two"],["z","z"],["01","one"],["4294967295","max"]]);
add("lone UTF16 keys and content",[system(JSON.parse('{"\\ud800":"old","toString":"own"}'),[tool("\udfff","\ud800",JSON.parse('{"\\ud800":"\\udfff"}'))])],[["\ud800","\udfff"],["toString","own"]],[tool("\udfff","\ud800",JSON.parse('{"\\ud800":"\\udfff"}'))]);
assert.deepEqual(add("prototype minimal addition drops its key",[],[["__proto__","value"]]),[]);
assert.deepEqual(add("prototype minimal update drops its key",[system(JSON.parse('{"__proto__":"old"}'))],[["__proto__","new"]]),[]);
assert.deepEqual(add("prototype minimal deletion drops its key",[system(JSON.parse('{"__proto__":"old"}'))],[]),[]);
add("prototype survives order rewrite",[system(JSON.parse('{"a":"1","__proto__":"old"}'))],[["__proto__","new"],["a","1"]]);
for(const desired of [[],[["a","same"]],[["__proto__","own"],["\ud800","\udfff"]]]){
 add(`head baseline ${cases.length}`,[system({a:"same"},[a])],desired,[a,b],entry(4,"summary"),[entry(2),entry(4,"summary")]);
 add(`head baseline omits retained systems on both sides ${cases.length}`,[system({a:"same"})],desired,[a],entry(10,"summary"),[entry(2),entry(4),entry(8,"note"),entry(9)]);
 add(`later system uses normal patch ${cases.length}`,[system({a:"same"},[a])],desired,[a],entry(4,"summary"),[entry(2),entry(4,"summary"),entry(6)]);
}
const text=JSON.stringify({reference:pin,cases},null,2)+"\n";
const path=fileURLToPath(new URL("../../crates/pi-durable/tests/fixtures/prompt.json",import.meta.url));
if(process.argv.includes("--check"))assert.equal(await readFile(path,"utf8"),text,"Regenerate the reference prompt fixture");else await writeFile(path,text);
console.log(`Reference prompt: ${cases.length} planner/replay cases`);
