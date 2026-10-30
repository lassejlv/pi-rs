import {chromium} from "playwright-core";
import {createServer} from "node:http";
import {readFile} from "node:fs/promises";
import {join,extname} from "node:path";
import assert from "node:assert/strict";
const root=new URL(".",import.meta.url).pathname;
const types={".html":"text/html",".js":"text/javascript",".wasm":"application/wasm",".json":"application/json"};
const server=createServer(async(req,res)=>{try{const p=join(root,decodeURIComponent(new URL(req.url,"http://x").pathname));const body=await readFile(p);res.writeHead(200,{"content-type":types[extname(p)]??"application/octet-stream"});res.end(body);}catch{res.writeHead(404);res.end();}});
await new Promise(ok=>server.listen(0,"127.0.0.1",ok));
const browser=await chromium.launch({executablePath:process.env.CHROME,headless:true});
try{
 const page=await browser.newPage();page.on("console",m=>console.log("page:",m.text()));
 await page.goto(`http://127.0.0.1:${server.address().port}/index.html`);
 const out=JSON.parse(await Promise.race([page.evaluate(()=>window.result),new Promise((_,no)=>setTimeout(()=>no(new Error("page did not finish within 120 s")),120000))]));
 if(out.error)throw new Error(out.error);
 const fixture=JSON.parse(await readFile(join(root,"fixture.json"),"utf8"));
 // The reference conformance traces, replayed against IndexedDbStorage<WebIdbConnection>.
 for(const [variant,result] of [["plain",out.plain],["across reopen",out.reopen]]){
  for(const failure of result.failed)console.log(`FAIL ${variant}: ${failure.name}: ${failure.error}`);
  assert.equal(result.failed.length,0,`${result.failed.length} ${variant} conformance cases failed`);
  console.log(`IndexedDbStorage conformance ${variant}: ${result.passed} cases passed in Chrome`);
 }
 console.log(`Session over IndexedDbStorage (executor on the microtask queue): concurrent commits, snapshot, close, reopen ok (conversation ${out.session.conversation})`);
 console.log(`Harness turn over IndexedDbStorage (faux provider, tool call, timer-paced stream, close/reopen): transcript ${out.harness.transcript.join(" > ")}`);
 assert.equal(out.timers.cleared,6,"dropped wasm sleeps must clear their setTimeout (4 dropped + 2 timeouts), fired ones none");
 console.log(`wasm timer seams: ${out.timers.cleared} unfired sleeps cleared their setTimeout on drop; fired sleeps cleared none`);
 // Rows the Rust adapter wrote, as browser JavaScript reads them, equal the reference rows.
 assert.deepStrictEqual(out.rustWritten,fixture.finalStores);
 console.log(`Chrome ${browser.version()}: ${out.rust.differentialChecks} browser/model differential checks; Rust-written stores equal reference (${Object.values(fixture.finalStores).reduce((n,s)=>n+s.rows.length,0)} rows, schema incl.); Rust read ${out.js.jsWrittenRows} JS-written rows`);
}finally{await browser.close();server.close();}
