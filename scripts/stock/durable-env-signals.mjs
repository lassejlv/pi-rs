// Invocation cancellation for environment capabilities shared by both stock adapters.
import {BACKGROUND_CONTEXT,withAbortSignal} from "@earendil-works/chord/context";
import {nativeTransport} from "./durable-native-transport.mjs";

const controllers=new Map();
nativeTransport.event("environment.signal",args=>controllers.get(args.signal)?.abort(new Error("Operation aborted")));

export function environmentContext(args,parent=BACKGROUND_CONTEXT){
 const controller=new AbortController();
 controllers.set(args.signal,controller);
 if(args.aborted)controller.abort(new Error("Operation aborted"));
 const signal=parent.abortSignal?AbortSignal.any([parent.abortSignal,controller.signal]):controller.signal;
 return {context:withAbortSignal(signal,parent),release:()=>controllers.delete(args.signal)};
}
