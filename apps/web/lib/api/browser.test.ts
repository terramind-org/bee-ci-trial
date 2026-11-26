/**
 * The browser client loads the generated client on its first call and forwards every call to it
 * unchanged: unary methods as promises, streaming ones as async iterables.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const calls: string[] = [];
const created: unknown[] = [];
const createClient = (rpc: unknown) => (created.push(rpc), {
  change: { getChange: async (req: unknown) => ({ got: req }) },
  event: {
    subscribe: async function* (req: unknown) {
      calls.push("subscribe " + JSON.stringify(req));
      yield { seq: "1" };
      yield { seq: "2" };
    },
  },
  readText: async () => ({ text: "hello", size: 5, binary: false }),
  diffAll: async () => [],
});
vi.mock("./client", () => ({ createClient }));
vi.mock("./http", () => ({ httpRpc: (o: unknown) => o }));

describe("browserClient", () => {
  beforeEach(() => {
    calls.length = 0;
  });

  it("does not load the client until a method is called", async () => {
    const { browserClient } = await import("./browser");
    const api = browserClient();
    void api.change;
    expect(created).toEqual([]);
    await expect(api.change.getChange({ repo: { fullName: "acme/api" }, id: "c1" })).resolves.toEqual({ got: { repo: { fullName: "acme/api" }, id: "c1" } });
    expect(created).toEqual([{ baseUrl: "/api/connect" }]);
  });

  it("returns streaming methods as iterables that start on iteration", async () => {
    const { browserClient } = await import("./browser");
    const it = browserClient().event.subscribe({ repo: { fullName: "acme/api" }, fromSeq: "0" });
    expect(typeof it[Symbol.asyncIterator]).toBe("function");
    expect(calls).toEqual([]);
    const seen: string[] = [];
    for await (const e of it) seen.push(e.seq);
    expect(seen).toEqual(["1", "2"]);
    expect(calls).toEqual(['subscribe {"repo":{"fullName":"acme/api"},"fromSeq":"0"}']);
  });

  it("forwards the whole-file helper and reuses one client", async () => {
    const { browserClient } = await import("./browser");
    expect(browserClient()).toBe(browserClient());
    await expect(browserClient().readText({ repo: { fullName: "acme/api" }, at: { expr: "main" }, path: "" })).resolves.toEqual({ text: "hello", size: 5, binary: false });
    expect(created).toHaveLength(1);
  });
});
