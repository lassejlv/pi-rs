// The alarm half of upstream test-workers/worker.ts (`arm`) through the Rust alarm host, in workerd.
import { runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { alarmAt } from "./worker.mjs";

const fresh = () => env.SESSIONS.get(env.SESSIONS.newUniqueId());
const HEARTBEAT = 60_000;

describe("the alarm a host arms for a wake", () => {
	it("is the earliest of the heartbeat and the sleep, in wall-clock time", () => {
		expect(alarmAt(false, undefined, 1_000, 0)).toBeUndefined();
		expect(alarmAt(true, undefined, 1_000, 0)).toBe(1_000 + HEARTBEAT);
		expect(alarmAt(false, 5_000, 1_000, 2_000)).toBe(3_000);
		expect(alarmAt(true, 5_000, 1_000, 0)).toBe(5_000);
		expect(alarmAt(true, 500_000, 1_000, 0)).toBe(1_000 + HEARTBEAT);
	});

	it("sets, replaces and deletes the object's real alarm", async () => {
		await runInDurableObject(fresh(), async (instance: any, state: any) => {
			const now = Date.now();
			await instance.arm(false, now + 600_000, now, 0);
			expect(await state.storage.getAlarm()).toBe(now + 600_000);
			await instance.arm(true, undefined, now, 0);
			expect(await state.storage.getAlarm()).toBe(now + HEARTBEAT);
			await instance.arm(false, undefined, now, 0);
			expect(await state.storage.getAlarm()).toBeNull();
		});
	});

	it("keeps an earlier alarm when arming the fallback, and replaces a later one", async () => {
		await runInDurableObject(fresh(), async (instance: any, state: any) => {
			const now = Date.now();
			await state.storage.setAlarm(now + 1_000);
			await instance.armFallback(now);
			expect(await state.storage.getAlarm()).toBe(now + 1_000);
			await state.storage.setAlarm(now + 10 * HEARTBEAT);
			await instance.armFallback(now);
			expect(await state.storage.getAlarm()).toBe(now + HEARTBEAT);
			await state.storage.deleteAlarm();
			await instance.armFallback(now);
			expect(await state.storage.getAlarm()).toBe(now + HEARTBEAT);
		});
	});
});
