// The Durable Object class workerd instantiates. All behaviour lives in the Rust
// `Core` (worker/src/lib.rs); this class owns only the Durable Object lifecycle,
// as `TestSession` does in upstream's test-workers/worker.ts.
import { DurableObject } from "cloudflare:workers";
import { Core, WakeCore, initSync } from "./pkg/pi_durable_workerd.js";
// Wrangler's default `CompiledWasm` rule turns this import into a WebAssembly.Module.
import wasm from "./pkg/pi_durable_workerd_bg.wasm";

initSync({ module: wasm });

export { alarmAt, conformanceCases, migrationScenario, runConformanceCase } from "./pkg/pi_durable_workerd.js";
export { timerPolicy } from "./pkg/pi_durable_workerd.js";
export { harnessTurnBegin, harnessTurnResume } from "./pkg/pi_durable_workerd.js";

export class TestSession extends DurableObject {
	core;

	constructor(ctx, env) {
		super(ctx, env);
		void ctx.blockConcurrencyWhile(async () => {
			this.core = await Core.open(ctx.storage);
		});
	}

	/** Arm the alarm for a wake, through the Rust alarm host. */
	arm(active, at, now, offset) {
		return this.core.arm(active, at, now, offset);
	}
	armFallback(now) {
		return this.core.armFallback(now);
	}
	async alarm() {}
}

/**
 * `TestSession` of upstream's test-workers/worker.ts: a Harness opened in the constructor and woken by
 * its alarm. Its behaviour is the Rust `WakeCore` (worker/src/wake.rs).
 */
export class WakeSession extends DurableObject {
	core;

	constructor(ctx, env) {
		super(ctx, env);
		void ctx.blockConcurrencyWhile(async () => {
			this.core = await WakeCore.open(ctx.storage, (reason) => ctx.abort(reason));
		});
	}

	now() {
		return this.core.now();
	}
	advance(ms) {
		this.core.advance(ms);
	}
	script(responses) {
		this.core.script(responses);
	}
	wake() {
		return JSON.parse(this.core.wake());
	}
	wakes() {
		return JSON.parse(this.core.wakes());
	}
	hanging() {
		return this.core.hanging();
	}
	submit(text, requestId) {
		return this.core.submit(text, requestId);
	}
	answer(text, requestId) {
		return this.core.answer(text, requestId);
	}
	remind(at) {
		return this.core.remind(at);
	}
	taskStatus(id) {
		return this.core.taskStatus(id);
	}
	waitForTask(id) {
		return this.core.waitForTask(id);
	}
	close() {
		return this.core.close();
	}
	async alarm() {
		await this.core.alarm();
	}
}

export default {
	fetch: () => new Response("pi-durable workerd test worker"),
};
