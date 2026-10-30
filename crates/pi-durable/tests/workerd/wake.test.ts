// Upstream test-workers/wake.test.ts against the Rust Durable Object.
import { evictDurableObject, runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { timerPolicy } from "./worker.mjs";

const fresh = () => env.WAKE.get(env.WAKE.newUniqueId());

/** Poll `check` inside the object until it holds. */
async function eventually(stub: any, check: (instance: any, state: any) => Promise<boolean>): Promise<void> {
	for (let attempt = 0; attempt < 200; attempt++) {
		if (await runInDurableObject(stub, check)) return;
		await new Promise((resolve) => setTimeout(resolve, 10));
	}
	throw new Error("Condition not reached");
}

describe("a Harness woken by its Durable Object alarm", () => {
	it("sleeps through an eviction and resumes the task from its alarm", async () => {
		const stub = fresh();
		const sleep = 600_000;
		const { id, at } = await runInDurableObject(stub, async (instance: any) => {
			const at = instance.now() + sleep;
			return { id: await instance.remind(at), at };
		});
		await eventually(
			stub,
			async (instance, state) => (await instance.taskStatus(id)) === '"sleeping"' && (await state.storage.getAlarm()) === at,
		);
		await runInDurableObject(stub, async (instance: any) => {
			expect(instance.wake()).toEqual({ active: false, at });
			expect(instance.wakes().at(-1)).toEqual({ active: false, at });
		});

		await evictDurableObject(stub);
		// A new instance: the sleep is read from storage, and the alarm stays.
		await runInDurableObject(stub, async (instance: any, state: any) => {
			expect(instance.wake()).toEqual({ active: false, at });
			expect(await state.storage.getAlarm()).toBe(at);
			// Time passes while the object sleeps.
			instance.advance(sleep);
		});
		await evictDurableObject(stub);
		expect(await runDurableObjectAlarm(stub)).toBe(true);

		const woke = await runInDurableObject(stub, (instance: any) => instance.waitForTask(id));
		expect(woke).toBeGreaterThanOrEqual(at);
		// Nothing is left to do: the alarm is gone.
		await eventually(stub, async (_instance, state) => (await state.storage.getAlarm()) === null);
	});

	it("recovers a turn an eviction cut off mid-stream through the heartbeat alarm", async () => {
		const stub = fresh();
		await runInDurableObject(stub, async (instance: any, state: any) => {
			instance.script(["hang", "recovered"]);
			await instance.submit("hi", "turn-1");
			await instance.hanging();
			expect(instance.wake().active).toBe(true);
			// The heartbeat alarm armed before the input was admitted.
			expect(await state.storage.getAlarm()).not.toBeNull();
		});

		await evictDurableObject(stub);
		expect(await runDurableObjectAlarm(stub)).toBe(true);

		// The same request ID finds the submission admitted before the eviction.
		const answer = await runInDurableObject(stub, (instance: any) => instance.answer("hi", "turn-1"));
		expect(answer).toBe("recovered");
		await eventually(stub, async (_instance, state) => (await state.storage.getAlarm()) === null);
	});

	it("can evict an object whose Harness keeps idle documents and contexts in memory", async () => {
		const stub = fresh();
		await runInDurableObject(stub, async (instance: any) => {
			instance.script(["answer"]);
			await instance.submit("hi", "turn-1");
			await instance.answer("hi", "turn-1");
		});
		// Retention timers would keep the object running; Workers decline them.
		expect(timerPolicy()).toBe(true);
		const evicted = await Promise.race([
			evictDurableObject(stub).then(() => "evicted"),
			new Promise((resolve) => setTimeout(() => resolve("still running"), 2_000)),
		]);
		expect(evicted).toBe("evicted");
	});
});
