// Generate data fixtures with the pinned reference. TypeScript stays outside this repo.
import { execFileSync } from "node:child_process";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { resolve, dirname } from "node:path";
import { pathToFileURL, fileURLToPath } from "node:url";

const pin = "5e63241d0294ed3fcfc4388c737a8ea27e09c74e";
const reference = resolve(process.argv[2] ?? "../pi-mono");
if (execFileSync("git", ["rev-parse", "HEAD"], { cwd: reference, encoding: "utf8" }).trim() !== pin) {
  throw new Error("Reference commit differs from the durable pin");
}
const d = await import(pathToFileURL(resolve(reference, "packages/chord/src/delta/index.ts")));
const encoder = d.encoder();
const streams = [];
let state = { text: "", rows: [], nested: { count: 0 }, tail: [] };
const batches = [[["r", state]]];
for (let i = 0; i < 128; i++) {
  batches.push([
    ["a", ["text"], `${i}:\u{1d11e}\n`],
    ["t", ["text"], i % 4 === 0 ? 2 : 0],
    ["s", ["nested", "count"], i],
    ["p", ["rows"], i, 0, [{ value: i }]],
    ["s", ["rows", i, "value"], i * 2],
    ["p", ["tail"], 0, 0, [i]],
  ]);
  if (i % 11 === 0) batches.push([["p", ["tail"], 1, 1000000, []]]);
  if (i === 63) batches.push([["r", { text: "reset", rows: Array.from({length:64}, (_, j) => ({value:j})), nested: {count:63}, tail:[] }]]);
}
for (const ops of batches) {
  state = d.applyImmutable(state, ops);
  streams.push({ ops, wire: encoder.encode(ops), value: state });
}
const application = [
  { initial: [1,2,3], ops: [["p",[],1,1,[4]], ["m",[],[2,0,1]]] },
  { initial: {values:[1]}, ops: [["s",["values",1],9], ["d",["values",0]]] },
  { initial: {}, ops: [["s",[0],"numeric object key"], ["d",["absent"]]] },
  { initial: {}, ops: [["s",["v"],JSON.parse('{"__proto__":{"z":1}}')]] },
  { initial: {v:"a\u{1d11e}b"}, ops: [["t",["v"],3]] },
  { initial: [1], ops: [["p",[],999,0,[2,3]]] },
];
for (const item of application) item.value = d.applyImmutable(item.initial, item.ops);
const invalidOps = [
  null, {}, [], ["ZZZ",["v"],9], ["s","v",9], ["s",[],9], ["d",[]],
  ["s",["constructor","prototype","x"],1], ["s",["v",-1],1],
  ["s",["v",0.5],1], ["p",[],0,0,"bad"], ["p",[],-1,0,[]],
  ["p",[],0,-1,[]], ["m",[],[0,0]], ["m",[],[1]],
  ["a",["v"],3], ["t",["v"],-1], ["t",["v"],0.5],
  ["s",1], ["d"], ["a","x"], ["t",2], ["p",0,0,[]], ["#",0,["v"]],
];
const errors = invalidOps.map(op => {
  try { d.assertValidOp(op); throw new Error("Expected invalid op"); }
  catch (error) { return { op, message: error.message }; }
});
const invalidWire = [
  [["a","x"]], [["#",0,["__proto__"]]], [["s",0,1]],
  [["s",[],1]], [["#",-1,[]]], [["a",[],3]], [["t",[],0.5]],
  [["p",[],0,0,"bad"]], [["m",[],[0,0]]], [["s","bad",1]],
];
const wireErrors = invalidWire.map(wire => {
  try { d.decoder().decode(wire); throw new Error("Expected invalid wire"); }
  catch (error) { return { wire, message: error.message }; }
});
const applicationErrors = [
  { initial:{values:[1]}, ops:[["s",["values",3],9]] },
  { initial:{values:[]}, ops:[["s",["values",4294967290],9]] },
  { initial:{values:[1]}, ops:[["s",["values","0"],9]] },
  { initial:{values:[1]}, ops:[["d",["values",1]]] },
  { initial:{v:1}, ops:[["a",["v"],"x"]] },
  { initial:{v:1}, ops:[["a",["missing"],"x"]] },
  { initial:{}, ops:[["s",["absent","x"],1]] },
  { initial:[], ops:[["m",[],[1,0]]] },
].map(item => {
  try { d.apply(structuredClone(item.initial), item.ops); throw new Error("Expected application error"); }
  catch (error) { return { ...item, message:error.message }; }
});
const overlaps = [
  ["abcdefgh","defghxyz",65536,64,8], ["abcdef","defghi",0,64,8],
  ["a".repeat(100),"a".repeat(90)+"b",100,64,8],
  ["a\u{1d11e}","\u{1d11e}b",65536,64,8], ["","a",100,64,8],
].map(args => ({ args, value:d.overlap(...args) }));
const diffs = [
  [{a:1,remove:true},{a:2,b:3}],
  [{text:"abcdefgh"},{text:"abcdefghij"}],
  [{text:"abcdefghij"},{text:"defghijxyz"}],
  [[1,2,3],[1,4,2,3]], [[1,2,3],[1,3]],
  [[1,2,3],[3,1,2]],
  [{rows:[{n:1},{n:2},{n:3}]},{rows:[{n:1},{n:4},{n:3}]}],
  [{text:"\ud834\udd1e"},{text:"\udd1e"}],
  [{text:"\ud834"},{text:"\ud834\udd1e"}],
  [JSON.parse('{"__proto__":{"x":1}}'),JSON.parse('{"__proto__":{"x":2}}')],
  [{nested:{count:1,items:[1,2]}},{nested:{items:[1,2],count:1}}],
  [null,{value:true}], [[],Array.from({length:30},(_,i)=>i)],
].map(([before,after])=>({before,after,ops:d.diffRevisions(before,after)}));
const output = JSON.stringify({reference:pin, streams, application, errors, wireErrors, applicationErrors, overlaps, diffs}, null, 2) + "\n";
const target = fileURLToPath(new URL("../../crates/pi-chord/tests/fixtures/delta.json", import.meta.url));
if (process.argv.includes("--check")) {
  if (await readFile(target, "utf8") !== output) throw new Error("Delta fixtures differ from pinned reference");
} else {
  await mkdir(dirname(target), {recursive:true});
  await writeFile(target, output);
}
console.log(`Verified ${streams.length} stream batches, ${application.length} applications, ${errors.length + wireErrors.length + applicationErrors.length} rejections`);
const nativeIndex = process.argv.indexOf("--verify-native");
if (nativeIndex !== -1) {
  const native = JSON.parse(await readFile(process.argv[nativeIndex+1],"utf8"));
  for (const row of native) {
    const actual = d.applyImmutable(row.before,row.ops);
    if (JSON.stringify(actual) !== JSON.stringify(row.after)) {
      // Key order is not revision equality; compare through the reference diff.
      if (d.diffRevisions(actual,row.after).length !== 0) throw new Error("Reference could not replay native delta");
    }
  }
  console.log(`Reference replayed ${native.length} native-generated deltas`);
}
