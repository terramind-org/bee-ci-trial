import { beforeEach, describe, expect, it, vi } from "vitest";
import { createClient, type BeeClient } from "@/lib/api/client";
import { FixtureStore, fixtureRpc } from "@/lib/api/fixture";

const current: { client: BeeClient | null; store: FixtureStore | null } = { client: null, store: null };
vi.mock("@/lib/api/server", () => ({
  serverClient: async () => current.client,
  sessionState: async () => ({ me: (await current.client!.auth.whoAmI({})).principal ?? null, error: null }),
}));

const { InboxCount } = await import("../../app/inbox-count");
const { loadNeeds, needCount } = await import("./load");

/** The number a rendered rail badge shows, read from its screen-reader text. */
async function railNumber(): Promise<number> {
  const el = await InboxCount();
  if (!el) return 0;
  const text = JSON.stringify(el);
  const m = /(\d+) need you|1 needs you/.exec(text);
  return m ? Number(m[1] ?? 1) : NaN;
}

describe("the rail's Inbox count", () => {
  let tick = 0;
  beforeEach(() => {
    tick = 0;
    current.store = new FixtureStore({ anchor: Date.UTC(2026, 9, 18, 9, 0, 0), clock: () => tick, live: false });
    current.client = createClient(fixtureRpc(current.store, "mara-token-0001"));
  });

  it("is the number the Inbox page reads from the same snapshot", async () => {
    const scan = await loadNeeds();
    expect(needCount(scan)).toBeGreaterThan(0);
    expect(await railNumber()).toBe(needCount(scan));
  });

  it("is the same on every request while the world's clock stands still", async () => {
    const first = needCount(await loadNeeds());
    expect(needCount(await loadNeeds())).toBe(first);
  });
});
