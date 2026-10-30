// Test-only bindings. Rust owns built-in tool behavior and file queue admission.
import {BACKGROUND_CONTEXT} from "@earendil-works/chord/context";
import {nativeTransport} from "./durable-native-transport.mjs";
import {handle,objectFor} from "./durable-native-handles.mjs";
import {environmentContext} from "./durable-env-signals.mjs";
let environmentCalls=0,functions=0;
const declarations=new Map(),readers=new Set();
const scalar=n=>typeof n==="number"&&!Number.isFinite(n)?String(n):n;
function environment(env){return env===undefined?undefined:{handle:handle(env),namespace:env.id,cwd:env.cwd};}
nativeTransport.register("environment.call",async(args,context)=>{
 environmentCalls++;
 const scope=environmentContext(args,context??BACKGROUND_CONTEXT);
 try{
 const env=objectFor(args.environment),values=[...args.args],method=args.method==="writeTextFile"?"writeFile":args.method;
 if(args.method==="writeFile"||args.method==="appendFile")values[1]=Uint8Array.from(Buffer.from(values[1],"base64"));
 if(method==="exec"&&values[1]?.onOutput!==undefined){const callback=values[1].onOutput;values[1].onOutput=(text,_context,info)=>nativeTransport.sync("environment.output",{callback,text,info});}
 const result=await env[method](...values,scope.context);
 if(method==="cleanup"||method==="close"){readers.delete(args.environment);return {ok:true,value:null};}
 if(result.ok){
  if(method==="readBinaryFile"||method==="read")return {ok:true,value:Buffer.from(result.value).toString("base64")};
  if(method==="openBinaryReader"){const id=handle(result.value);readers.add(id);return {ok:true,value:id};}
  return result;
 }
 return {ok:false,error:{code:result.error.code,message:result.error.message,path:result.error.path,spillPath:result.error.spillPath}};
 }finally{scope.release();}
});
nativeTransport.register("builtin.function",callback=>{functions++;return objectFor(callback)();});
nativeTransport.register("builtin.api",args=>{
 const api=objectFor(args.api);
 if(args.method==="environment")return environment(api.env)??null;
 if(args.method==="window")return api.outputWindow??null;
 if(args.method==="output"){api.output(...args.args);return null;}
 if(args.method==="diagnostic"){api.diagnostic(args.args);return null;}
 if(args.method==="retainedOutput")return api.retainedOutput();
 throw new Error(`Unknown built-in API capability ${args.method}`);
});
function modelInput(model){const resize=model?.inputLimits?.images?.resize;return {vision:model?.input.includes("image")??true,maxWidth:scalar(resize?.maxWidth),maxHeight:scalar(resize?.maxHeight),maxBytes:scalar(resize?.maxBytes)};}
nativeTransport.register("builtin.imageModel",async(id,context)=>{const api=objectFor(id),ref=(await api.agent(context??BACKGROUND_CONTEXT)).model;return modelInput(ref===undefined?undefined:api.models.getModel(ref.provider,ref.modelId));});
nativeTransport.register("builtin.imagePrepare",args=>objectFor(args.processor).prepare(Uint8Array.from(Buffer.from(args.bytes,"base64")),args.mime,args.limits));
nativeTransport.register("builtin.prepare",async(args,context)=>{await objectFor(args.options).prepare(args.execution,objectFor(args.api),context??BACKGROUND_CONTEXT);return args.execution;});
function tool(name,options={}){
 const wire={...(options.commandPrefix===undefined?{}:{commandPrefix:options.commandPrefix}),...(options.programs===undefined?{}:{programs:options.programs}),...(options.images===undefined?{}:{images:handle(options.images)}),...(options.prepare===undefined?{}:{prepare:handle(options)})};
 const key=`${name}:${options.images!==undefined}`;
 if(!declarations.has(key))declarations.set(key,nativeTransport.sync("builtin.describe",{tool:name,options:wire}));
 return {...declarations.get(key),...(name==="edit"?{prepareArguments:args=>nativeTransport.sync("builtin.prepareEdit",{args})}:{}),execute:async(args,api,context)=>nativeTransport.request(`builtin.${name}`,{args:{...args,...("offset" in args?{offset:scalar(args.offset)}:{}),...("limit" in args?{limit:scalar(args.limit)}:{}),...("timeout" in args?{timeout:scalar(args.timeout)}:{})},options:wire,api:handle(api),...(name==="write"||name==="edit"?{environment:environment(api.env)}:{})},context)};
}
export const createWriteTool=()=>tool("write");
export const createEditTool=()=>tool("edit");
export const createReadTool=options=>tool("read",options);
export const createBashTool=options=>tool("bash",options);
export const createPowerShellTool=options=>tool("powershell",options);
export function withFileMutationQueue(env,path,fn,context){return nativeTransport.request("builtin.mutation",{environment:environment(env),path,callback:handle(fn)},context);}
function pure(operation,args){return nativeTransport.sync("builtin.pure",{operation,args});}
export const detectSupportedImageMimeType=bytes=>pure("image.detect",{bytes:Buffer.from(bytes).toString("base64")})??undefined;
export const imageDimensions=(bytes,mime)=>pure("image.dimensions",{bytes:Buffer.from(bytes).toString("base64"),mime})??undefined;
export const DEFAULT_IMAGE_LIMITS={maxWidth:2000,maxHeight:2000,maxBytes:4.5*1024*1024};
export const INLINE_IMAGE_TYPES=new Set(["image/png","image/jpeg","image/gif","image/webp"]);
export const toBase64=bytes=>pure("base64",{bytes:Buffer.from(bytes).toString("base64")});
export const base64Length=bytes=>pure("base64.length",{bytes:scalar(bytes)});
export async function shutdown(){if(readers.size)throw new Error(`Native read tools left ${readers.size} readers open`);const native=await nativeTransport.request("builtin.stats");await nativeTransport.close();return {...native,environmentCalls,functions,activeReaders:readers.size};}
export const exifOrientation=bytes=>pure("image.exif",{bytes:Buffer.from(bytes).toString("base64")});
function nativeImages(options={}){
 const worker=options.worker!==false,processor=nativeTransport.sync("builtin.imageNew",{worker,idleMs:scalar(options.idleMs??30000)});
 return {prepare:async(bytes,mime,limits)=>{
  const args={processor,bytes:Buffer.from(bytes).toString("base64"),mime,limits:{maxWidth:scalar(limits.maxWidth),maxHeight:scalar(limits.maxHeight),maxBytes:scalar(limits.maxBytes)}};
  return (worker?await nativeTransport.request("builtin.nativeImagePrepare",args):nativeTransport.sync("builtin.nativeImagePrepare",args))??undefined;
 }};
}
export async function createNodePhotonImages(options){return nativeImages(options);}
export function createPhotonImages(_wasm){return nativeImages({worker:false});}
