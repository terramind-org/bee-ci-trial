import { beforeEach, describe, expect, it, vi } from "vitest";
import { createClient, type BeeClient } from "@/lib/api/client";
import { FixtureStore, fixtureRpc } from "@/lib/api/fixture";

// The loader reads through the request's server client; here that is the demo user on a fresh
// fixture world, so the scan runs over the same data the fixture app shows.
const current: { client: BeeClient | null; store: FixtureStore | null } = { client: null, store: null };
vi.mock("@/lib/api/server", () => ({
  serverClient: async () => current.client,
  sessionState: async () => ({ me: (await current.client!.auth.whoAmI({})).principal ?? null, error: null }),
}));

const { loadNeeds, needCount } = await import("./load");
const { needsOf } = await import("./needs");

describe("loadNeeds", () => {
  beforeEach(() => {
    current.store = new FixtureStore({ anchor: Date.UTC(2026, 9, 18, 9, 0, 0), clock: () => 0, live: false });
    current.client = createClient(fixtureRpc(current.store, "mara-token-0001"));
  });

  it("scans the caller's repositories and classifies with needsOf, the function the rail count reads", async () => {
    const scan = await loadNeeds();
    expect(scan.me.display).toBeTruthy();
    expect(scan.checked.length).toBeGreaterThan(0);
    expect(scan.failed).toEqual([]);
    expect(scan.facts.length).toBe(Math.min(scan.open, 60));
    expect(scan.needs.length).toBeGreaterThan(0);
    // Every listed item is something only this person can move: never their own post.
    for (const n of scan.needs) if (n.kind === "post") expect(n.post?.author?.id).not.toBe(scan.me.id);
    // Recomputing from the scan's facts gives the same open-change needs.
    expect(needsOf(scan.facts, [], scan.me).map((n) => n.change.id)).toEqual(scan.needs.filter((n) => n.kind !== "post").map((n) => n.change.id));
  });

  it("delivers tripwire and concern reasons to the person who answers, and includes both in the rail count", async () => {
    const scan = await loadNeeds();
    expect(scan.safetyFailed).toEqual([]);
    expect(scan.safety).toMatchObject([
      { kind: "tripwire", repo: "acme/api", reason: expect.stringContaining("customer identifiers"), answers: { id: scan.me.id } },
      { kind: "concern", repo: "acme/api", reason: expect.stringContaining("redacted session"), answers: { id: scan.me.id } },
    ]);
    expect(needCount(scan)).toBe(scan.needs.length + 2);
  });

  it("lists an agent's merged post that waits for this person's approval, in place of the review", async () => {
    // The seed's agent draft on "Rotate session ids on role change": merging it leaves the post
    // waiting for a person, and the demo user writes to that repository.
    const store = current.store!;
    const rotate = store.crByKey("rotate");
    rotate.state = "CHANGE_STATE_LANDED";
    store.posts.onLanded(store, rotate.id);

    const scan = await loadNeeds();
    const mine = scan.needs.filter((n) => n.change.id === rotate.id);
    expect(mine.map((n) => n.kind)).toEqual(["post"]);
    expect(mine[0]).toMatchObject({ repo: "acme/api", who: { display: "triage" }, post: { state: "POST_STATE_PENDING_APPROVAL", viewer: { canApprove: true } } });
  });
});
