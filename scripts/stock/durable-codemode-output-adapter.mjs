// The durable replay driver and VM remain original in this modular formatter gate.
import {nativeTransport} from "./durable-native-transport.mjs";
import {handle,objectFor} from "./durable-native-handles.mjs";
export const DEFAULT_MAX_OUTPUT_TOKENS=10000;
nativeTransport.register("codemode.write",({writer,name,data})=>objectFor(writer)(name,Array.isArray(data)?Uint8Array.from(data):data.text));
export function formatResult(result,options){
 const {write,...settings}=options;
 return nativeTransport.request("codemode.format",{result,options:settings,writer:handle(write)});
}
