import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("next/headers", () => ({ cookies: async () => ({ get: () => undefined }) }));

const { edition } = await import("./server");

afterEach(() => {
  vi.unstubAllEnvs();
  vi.unstubAllGlobals();
});

describe("edition()", () => {
  it("is cloud in the fixture world unless told otherwise", async () => {
    vi.stubEnv("BEE_ADAPTER", "fixture");
    expect(await edition()).toBe("cloud");
    vi.stubEnv("BEE_FIXTURE_EDITION", "enterprise");
    expect(await edition()).toBe("enterprise");
  });

  it("lets BEE_EDITION win, but only with a valid value", async () => {
    vi.stubEnv("BEE_ADAPTER", "fixture");
    vi.stubEnv("BEE_EDITION", "enterprise");
    expect(await edition()).toBe("enterprise");
    vi.stubEnv("BEE_EDITION", "bogus");
    expect(await edition()).toBe("cloud");
  });

  it("falls back to enterprise when the server is unreachable or leaves the field empty", async () => {
    vi.stubEnv("BEE_ADAPTER", "http");
    vi.stubGlobal("fetch", vi.fn(async () => { throw new Error("connection refused"); }));
    expect(await edition()).toBe("enterprise");
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify({ tier: "server" }), { status: 200, headers: { "content-type": "application/json" } })),
    );
    expect(await edition()).toBe("enterprise");
  });

  it("reads the edition a server reports", async () => {
    vi.stubEnv("BEE_ADAPTER", "http");
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify({ edition: "cloud" }), { status: 200, headers: { "content-type": "application/json" } })),
    );
    expect(await edition()).toBe("cloud");
  });
});
