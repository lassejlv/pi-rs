// Upstream test-workers/storage.test.ts against the Rust Durable Object: the storage conformance
// cases and the schema migration, run by Rust on the object's own SQLite.
import { runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { conformanceCases, migrationScenario, runConformanceCase } from "./worker.mjs";

const fresh = () => env.SESSIONS.get(env.SESSIONS.newUniqueId());

describe("SqliteStorage on Durable Object SQLite in workerd", () => {
	let suite = 0;
	for (const [index, name] of conformanceCases().entries()) {
		it(name, () =>
			runInDurableObject(fresh(), async (_instance, state) => {
				// Tables of their own, next to the ones the object's Harness opened.
				await runConformanceCase(index, state.storage, `conformance${suite++}_`);
			}),
		);
	}
});

describe("Durable Object SQLite schema", () => {
	it("migrates a version 1 schema to admit sleeping tasks, keeping tasks and indexes", async () => {
		await runInDurableObject(fresh(), async (_instance, state) => {
			await expect(migrationScenario(state.storage)).resolves.toBeUndefined();
		});
	});
});
