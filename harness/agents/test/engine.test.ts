// Unit tests of the assertion language, templates, ids and the Connect framing.

import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { repoRoot } from "../src/catalog";
import { blake3, blobId, toHex } from "../src/blake3";
import { ConnectClient, encodeEnvelopes, parseError, unframe, unixSocket } from "../src/connect";
import { evaluate, parsePath, select } from "../src/predicate";
import { idText, interpolate, type TemplateContext } from "../src/template";

describe("paths", () => {
  const doc = { a: { b: [{ c: 1 }, { c: 2 }, {}] }, snake_case: 5, lowerCamel: 6 };
  test("parse and select", () => {
    expect(parsePath("a.b[0].c")).toEqual(["a", "b", 0, "c"]);
    expect(parsePath("a.b[-1]")).toEqual(["a", "b", -1]);
    expect(select(doc, "a.b[1].c")).toBe(2);
    expect(select(doc, "a.b[-1]")).toEqual({});
    expect(select(doc, "a.b[*].c")).toEqual([1, 2]);
    expect(select(doc, "$")).toBe(doc);
  });
  test("accept either field name form", () => {
    expect(select(doc, "snakeCase")).toBe(5);
    expect(select(doc, "lower_camel")).toBe(6);
  });
  test("reject malformed paths", () => {
    expect(() => parsePath("a..b")).toThrow();
  });
});

describe("predicates", () => {
  const doc = {
    version: "18446744073709551615",
    n: 3,
    when: "2026-10-18T10:00:00.500Z",
    path: Buffer.from("src/a.py").toString("base64"),
    items: [{ k: "x", v: 1 }, { k: "y", v: 2 }],
    empty: [],
  };
  const ok = (p: Record<string, unknown>) => evaluate(doc, { path: "", ...p } as never).ok;
  test("int64 strings compare exactly", () => {
    expect(ok({ path: "version", gt: "18446744073709551614" })).toBe(true);
    expect(ok({ path: "version", eq: "18446744073709551615" })).toBe(true);
    expect(ok({ path: "n", eq: "3" })).toBe(true);
    expect(ok({ path: "n", lt: 2 })).toBe(false);
  });
  test("base64 decoding, contains, matches, in", () => {
    expect(ok({ path: "path", decode: "base64", eq: "src/a.py" })).toBe(true);
    expect(ok({ path: "items[*].k", contains: "y" })).toBe(true);
    expect(ok({ path: "items[*].k", not_contains: "z" })).toBe(true);
    expect(ok({ path: "path", decode: "base64", matches: "^src/" })).toBe(true);
    expect(ok({ path: "missing", in: ["a", null] })).toBe(true);
  });
  test("collections", () => {
    expect(ok({ path: "items", some: [{ path: "k", eq: "y" }, { path: "v", eq: 2 }] })).toBe(true);
    expect(ok({ path: "items", some: [{ path: "k", eq: "y" }, { path: "v", eq: 1 }] })).toBe(false);
    expect(ok({ path: "items", every: [{ path: "v", gte: 1 }] })).toBe(true);
    expect(ok({ path: "items", none: [{ path: "k", eq: "z" }] })).toBe(true);
    expect(ok({ path: "items", count_gte: 2, where: [{ path: "v", gt: 0 }] })).toBe(true);
    expect(ok({ path: "items", len: 2 })).toBe(true);
    expect(ok({ path: "empty", absent: true })).toBe(true);
    expect(ok({ path: "items", truthy: true })).toBe(true);
  });
  test("ids, prefixes and time windows", () => {
    expect(ok({ path: "items[0].k", prefix_of: "xyz" })).toBe(false); // too short to be an id prefix
    expect(evaluate({ id: "c1_0123456789ab" }, { path: "id", prefix_of: "c1_0123456789abcdef" }).ok).toBe(true);
    expect(ok({ path: "when", within: { from: "2026-10-18T10:00:00Z", ms: 1000 } })).toBe(true);
    expect(ok({ path: "when", within: { from: "2026-10-18T10:00:00Z", ms: 100 } })).toBe(false);
  });
  test("failure messages name the path and the values", () => {
    const r = evaluate(doc, { path: "n", eq: 4 });
    expect(r.ok).toBe(false);
    expect(r.message).toContain("n eq");
    expect(r.message).toContain("4");
  });
});

describe("fields proto3-JSON omits", () => {
  test("an absent number compares as 0 and an absent list has length 0", () => {
    expect(evaluate({}, { path: "failClosed", lte: 0 }).ok).toBe(true);
    expect(evaluate({}, { path: "misses", gte: 1 }).ok).toBe(false);
    expect(evaluate({}, { path: "alerts", len: 0 }).ok).toBe(true);
    expect(evaluate({}, { path: "alerts", len_gte: 1 }).ok).toBe(false);
    expect(evaluate({ alerts: [1] }, { path: "alerts", len: 0 }).ok).toBe(false);
  });
});

describe("prefix_of", () => {
  const full = "c1_" + "ab".repeat(32);
  const wire = Buffer.concat([Buffer.from([1, 3]), Buffer.alloc(32, 0xab)]).toString("base64");
  test("takes the text form, an abbreviation of it, and the wire form of the same id", () => {
    expect(evaluate({ c: full.slice(0, 16) }, { path: "c", prefix_of: full }).ok).toBe(true);
    expect(evaluate({ c: idText(wire) }, { path: "c", prefix_of: full }).ok).toBe(true);
    expect(evaluate({ c: wire }, { path: "c", prefix_of: idText(wire) }).ok).toBe(true);
    expect(evaluate({ c: "c1_cd" + "cd".repeat(10) }, { path: "c", prefix_of: full }).ok).toBe(false);
  });
});

describe("templates", () => {
  const ctx: TemplateContext = { vars: { repo: { fullName: "o/r", n: 7 }, list: [1, 2] }, now: new Date("2026-10-18T00:00:00Z"), runId: "run1" };
  test("variables keep their type when whole, join otherwise", () => {
    expect(interpolate<unknown>(ctx, "${repo.n}")).toBe(7);
    expect(interpolate<unknown>(ctx, "${list}")).toEqual([1, 2]);
    expect(interpolate(ctx, "x/${repo.fullName}/y")).toBe("x/o/r/y");
    expect(interpolate(ctx, "$${literal}")).toBe("${literal}");
  });
  test("functions", () => {
    expect(interpolate(ctx, "${b64:docs/a}")).toBe(Buffer.from("docs/a").toString("base64"));
    expect(interpolate(ctx, "${time:-24h}")).toBe("2026-10-17T00:00:00.000Z");
    const a = interpolate(ctx, "${changeid:x}") as string;
    expect(Buffer.from(a, "base64").length).toBe(16);
    expect(interpolate(ctx, "${changeid:x}")).toBe(a);
    expect(interpolate(ctx, "${changeid:y}")).not.toBe(a);
  });
  test("undefined variables and the admin token are refused", () => {
    expect(() => interpolate(ctx, "${nope}")).toThrow("undefined variable");
    expect(() => interpolate(ctx, "${env:BEE_HARNESS_ADMIN_TOKEN}")).toThrow();
    expect(() => interpolate(ctx, "${env:HOME}")).toThrow();
  });
  test("id text forms", () => {
    const cs = Buffer.concat([Buffer.from([1, 3]), Buffer.alloc(32, 0xab)]).toString("base64");
    expect(idText({ raw: cs })).toBe(`c1_${"ab".repeat(32)}`);
    expect(idText(Buffer.alloc(16, 1).toString("base64"))).toBe(`k_${"01".repeat(16)}`);
  });
});

describe("blake3 and blob ids", () => {
  const vectors = JSON.parse(readFileSync(join(repoRoot(), "testdata", "vectors", "blob-v1.json"), "utf8")).vectors as {
    id: string;
    value: { bytes_hex: string };
  }[];
  for (const v of vectors) {
    test(`golden vector ${v.id.slice(0, 12)}`, () => {
      const id = blobId(Buffer.from(v.value.bytes_hex, "hex"));
      expect(`b1_${toHex(id.subarray(2))}`).toBe(v.id);
      expect(id[0]).toBe(1);
      expect(id[1]).toBe(1);
    });
  }
  test("multi-chunk inputs are deterministic and length-sensitive", () => {
    const seen = new Set<string>();
    for (const n of [1023, 1024, 1025, 2048, 2049, 4096, 5000]) {
      const b = new Uint8Array(n).map((_, i) => i % 251);
      const h = toHex(blake3(b));
      expect(toHex(blake3(b))).toBe(h);
      seen.add(h);
    }
    expect(seen.size).toBe(7);
  });
});

describe("connect framing and errors", () => {
  test("a unix: endpoint is reached through its socket", async () => {
    expect(unixSocket("unix:/home/a/.bee/daemon.sock")).toBe("/home/a/.bee/daemon.sock");
    expect(unixSocket("unix:///run/t.sock")).toBe("/run/t.sock");
    expect(unixSocket("http://127.0.0.1:7420")).toBeUndefined();
    const sock = `/tmp/bee-harness-test-${process.pid}.sock`;
    const server = Bun.serve({ unix: sock, fetch: (r) => Response.json({ path: new URL(r.url).pathname }) });
    try {
      const r = await new ConnectClient(`unix:${sock}`).unary("ObjectService", "GetTierStats", {});
      expect(r.ok).toBe(true);
      expect((r.body as { path: string }).path).toBe("/bee.v1.ObjectService/GetTierStats");
    } finally {
      server.stop(true);
    }
  });
  test("envelopes round-trip with an end-stream frame", () => {
    const frames = unframe(encodeEnvelopes([{ a: 1 }, { b: 2 }], { error: { code: "x" } }));
    expect(frames.map((f) => f.flags)).toEqual([0, 0, 2]);
    expect(JSON.parse(new TextDecoder().decode(frames[1]!.payload))).toEqual({ b: 2 });
  });
  test("reasons come from ErrorDetail, else from the code", () => {
    const e = parseError(403, { code: "permission_denied", message: "m", details: [{ type: "bee.v1.ErrorDetail", debug: { reason: "ERROR_REASON_POLICY_DENIED" } }] });
    expect(e.reason).toBe("POLICY_DENIED");
    expect(parseError(400, { code: "failed_precondition" }).reason).toBe("INVALID_STATE");
    expect(parseError(503, "boom").reason).toBe("UNAVAILABLE");
  });
});
