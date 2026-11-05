// Resolve pinned package sources rather than stale build output in that checkout.
import {registerHooks} from "node:module";
import {existsSync} from "node:fs";
import {join} from "node:path";
import {pathToFileURL} from "node:url";
export function registerReferenceSource(reference){
 registerHooks({resolve(specifier,context,nextResolve){
  const match=/^@earendil-works\/(chord|pi-ai|pi-telemetry|pi-codemode)(?:\/(.*))?$/.exec(specifier);
  if(match){
   const base=join(reference,"packages",match[1].replace(/^pi-/,""),"src",match[2]??"index");
   const source=[base+".ts",join(base,"index.ts")].find(existsSync);
   if(!source)throw new Error(`Pinned reference source is missing for ${specifier}`);
   return nextResolve(pathToFileURL(source).href,context);
  }
  return nextResolve(specifier,context);
 }});
}
