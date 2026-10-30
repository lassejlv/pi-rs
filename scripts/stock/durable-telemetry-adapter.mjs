// Recording capabilities only. Span/metric/event decisions come from Rust.
import {BACKGROUND_CONTEXT,withTelemetry} from "@earendil-works/chord/context";
import {nativeTransport} from "./durable-native-transport.mjs";
import {handle,objectFor} from "./durable-native-handles.mjs";
const pending=new Map();
export function describeTelemetry(context){
 if(context===undefined)return null;
 return {id:handle(context),metrics:context.metrics===undefined?null:handle(context.metrics),propagation:context.propagation===undefined?null:handle(context.propagation)};
}
nativeTransport.contextAdapter({
 encode:context=>describeTelemetry((context??BACKGROUND_CONTEXT).telemetry),
 decode:(context,id)=>id===null||id===undefined?context:withTelemetry(objectFor(id),context??BACKGROUND_CONTEXT),
});
nativeTransport.register("telemetry.begin",args=>{
 let admitted;
 const promise=objectFor(args.context).startSpan({name:args.name,...(args.attributes===null?{}:{attributes:args.attributes}),links:args.links.map(link=>({context:objectFor(link.context),...(link.attributes===null?{}:{attributes:link.attributes})}))},span=>{
  admitted=describeTelemetry(span);
  const deferred=Promise.withResolvers();
  const queue=pending.get(admitted.id)??[];queue.push(deferred);pending.set(admitted.id,queue);
  return deferred.promise;
 });
 promise.catch(()=>{});
 if(admitted===undefined)throw new Error("Stock telemetry adapter did not admit its span synchronously");
 return admitted;
});
nativeTransport.register("telemetry.end",args=>{const queue=pending.get(args.span),deferred=queue?.shift();if(!queue?.length)pending.delete(args.span);if(!deferred)throw new Error("Unknown native telemetry span");args.error===null?deferred.resolve():deferred.reject(Object.assign(new Error(args.error.message),{name:args.error.name}));return null;});
nativeTransport.register("telemetry.link",args=>{objectFor(args.span).addLink({context:objectFor(args.context),...(args.attributes===null?{}:{attributes:args.attributes})});return null;});
nativeTransport.register("telemetry.event",args=>{objectFor(args.span).addEvent(args.name,args.attributes);return null;});
nativeTransport.register("telemetry.attributes",args=>{objectFor(args.span).setAttributes(args.attributes);return null;});
nativeTransport.register("telemetry.status",args=>{objectFor(args.span).setStatus(args.status);return null;});
nativeTransport.register("telemetry.inject",args=>objectFor(args.propagation).inject(objectFor(args.context))??null);
nativeTransport.register("telemetry.extract",args=>describeTelemetry(objectFor(args.propagation).extract(args.carrier??undefined)));
nativeTransport.register("telemetry.instrument",args=>handle(objectFor(args.metrics)[args.kind](args.name,Object.fromEntries(Object.entries(args.options).filter(([,value])=>value!==null)))));
nativeTransport.register("telemetry.add",args=>{objectFor(args.instrument).add(args.value,args.attributes);return null;});
nativeTransport.register("telemetry.record",args=>{objectFor(args.instrument).record(args.value,args.attributes,args.context===null?undefined:objectFor(args.context));return null;});
