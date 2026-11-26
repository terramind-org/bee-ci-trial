import { describe, expect, it } from "vitest";
import { isPolicyEjection } from "@/components/pull/live";
import { groupQueue } from "@/lib/queue";
import { createClient } from "../client";
import { FixtureStore, fixtureRpc } from "./index";

describe("fixture pull request motion events", () => {
  it("puts an old revision in Merged this week when the fixture server confirms its merge", async () => {
    let clock = 0;
    const store = new FixtureStore({ anchor: Date.UTC(2026, 9, 18, 9), clock: () => clock, live: false });
    const cr = store.crs.find((c) => c.title === "Add the us-east-2 cell")!;
    const repo = { fullName: "acme/infra" };
    const api = createClient(fixtureRpc(store, "mara-token-0001"));
    clock = 8 * 86_400_000;

    expect(await api.change.getChange({ repo, id: cr.id })).toMatchObject({ state: "CHANGE_STATE_OPEN" });
    store.land(cr, "mara");
    const list = await api.change.listChanges({ repo, state: "CHANGE_STATE_LANDED" });
    const merged = list.changes.find((change) => change.id === cr.id)!;
    const eventTime = store.events.findLast((event) => event.kind === "cr.landed" && event.change?.id === cr.id)?.time;
    expect(eventTime).toBeDefined();
    expect(merged.landedAt).toBeDefined();
    expect(Date.parse(merged.landedAt!)).toBe(Date.parse(eventTime!));
    expect(Date.parse(merged.revisions.at(-1)!.createdAt!) + 7 * 86_400_000).toBeLessThan(store.now());
    const groups = groupQueue([{ change: merged, landedAt: merged.landedAt, checks: [], reviews: [] }], null, store.now());
    expect(groups.find((group) => group.key === "landed")?.rows.map((row) => row.facts.change.id)).toEqual([cr.id]);
  });

  it("receives a policy refusal when a required check fails after fixture enqueue", async () => {
    const store = new FixtureStore({ anchor: Date.UTC(2026, 9, 18, 9), clock: () => 0, live: false });
    const api = createClient(fixtureRpc(store, "mara-token-0001"));
    const repo = { fullName: "acme/api" };
    const change = store.crByKey("webhooks");
    const { latestSeq } = await api.event.getEventLogInfo({ repo });
    expect((await api.change.listQueue({ repo, ref: "main" })).entries.some((entry) => entry.changeRequestId === change.id)).toBe(true);
    store.checks.get(store.current(change).head.idText)!.find((check) => check.name === "test")!.status = "CHECK_STATUS_FAILED";
    store.step();
    store.step();
    const { events } = await api.event.listEvents({ repo, fromSeq: latestSeq, kinds: ["merge_queue.left"] });
    expect(events.some((event) => event.change?.id === change.id && isPolicyEjection(event))).toBe(true);
    expect((await api.change.getChange({ repo, id: change.id })).state).toBe("CHANGE_STATE_OPEN");
  });
});
