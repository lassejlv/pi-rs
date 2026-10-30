// Registrations describe the Rust implementation; execution stays in its Harness.
import {nativeTransport} from "./durable-native-transport.mjs";
export const TOOL_SEARCH_TOOL_NAME="tool_search",DEFAULT_TOOL_SEARCH_LIMIT=8;
export function createToolSearchTool(){
 const declaration=nativeTransport.sync("builtin.describe",{tool:"tool_search"});
 return {...declaration,callers:["model"],replay:"safe",__piNativeBuiltin:"tool_search",execute(){throw new Error("Native tool search was bypassed");}};
}
export function createToolSearchExtension(){return {name:"tool-search",tools:[createToolSearchTool()]};}
