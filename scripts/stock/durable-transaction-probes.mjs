// Test-only prototype interception. Rust performs actual adoption and discard
// after these observers; the original stock spies may throw here to inject a fault.
import {nativeTransport} from "./durable-native-transport.mjs";
export class Transaction {
 adopt(_seq){}
 discard(){}
}
nativeTransport.register("session.transactionProbe",({method,seq})=>{
 const transaction=new Transaction();
 transaction[method](seq);
 return null;
});
