// All rendering executes in Rust. This module only supplies synchronous test IPC.
import {nativeTransport} from "./durable-native-transport.mjs";
const declaration=({name,description,inputSchema,outputSchema,signature})=>({name,description,inputSchema,outputSchema,signature});
export const schemaToType=(schema,options={})=>nativeTransport.sync("schemaToType",{schema,maxChars:options.maxChars});
export const renderToolSignature=(tool,options={})=>nativeTransport.sync("renderToolSignature",{tool:declaration(tool),maxChars:options.inputMaxChars});
export const renderToolSample=(tool,options={})=>nativeTransport.sync("renderToolSample",{tool:declaration(tool),maxChars:options.inputMaxChars});
export const renderDeclarations=options=>nativeTransport.sync("renderDeclarations",{tools:options.tools?.map(declaration),globals:options.globals?.map(declaration)});
