// The faux-provider Harness turn of tests/indexeddb_browser, inside workerd over the Durable
// Object's own SQLite: submit, tool call, timer-paced answer, eviction, transcript and receipt.
import { evictDurableObject, runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { harnessTurnBegin, harnessTurnResume } from "./worker.mjs";

const fresh = () => env.SESSIONS.get(env.SESSIONS.newUniqueId());

describe("a Harness conversation on Durable Object SQLite", () => {
	it("answers through a tool call and keeps the transcript and receipt across eviction", async () => {
		const stub = fresh();
		const begun = JSON.parse(await runInDurableObject(stub, (_instance, state) => harnessTurnBegin(state.storage)));
		expect(begun.transcript).toEqual(["pi.user", "pi.system", "pi.assistant", "pi.tool-result", "pi.assistant"]);
		await evictDurableObject(stub);
		const resumed = await runInDurableObject(stub, (_instance, state) =>
			harnessTurnResume(state.storage, begun.root, begun.submission, begun.transcript),
		);
		expect(resumed).toBe(begun.transcript.join(" > "));
	}, 30_000);
});
