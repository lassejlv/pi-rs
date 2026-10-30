// Rust supplies range decisions and transcript serialization. Original task
// tokens keep their identity; the native registry binds their executable phases.
export {CompactionTask,createCompaction,compactionPolicy,estimateContext,summarizedMessages} from "pi-durable-stock-reference-compaction";
import {nativeTransport} from "./durable-native-transport.mjs";
export function selectCut(view,budget){return nativeTransport.sync("compaction.select",{view:{...view,head:view.head??null},budget});}
export function serializeConversation(messages){return nativeTransport.sync("compaction.serialize",{messages});}
