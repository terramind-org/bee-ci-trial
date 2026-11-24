import { describe, expect, it } from "vitest";
import { createClient } from "../api/client";
import { FixtureStore, fixtureRpc } from "../api/fixture";
import { DIGEST_PARAM, evidenceUrl, parseEvidenceRequest } from "./evidence-url";
import { MAX_UPLOAD_BYTES, etagOf, parseRange, relay, relayUpload, serveBytes } from "./evidence-proxy";
import { Canvas, encodePng } from "./png";

const HEX = "a".repeat(64);

describe("evidence URLs", () => {
  it("rewrites a signed server URL to the same-origin route and carries the digest", () => {
    expect(evidenceUrl("/_evidence/r/ev1/main?exp=9&sig=ab", "sha256:" + HEX)).toBe(`/api/evidence/r/ev1/main?exp=9&sig=ab&${DIGEST_PARAM}=${HEX}`);
    expect(evidenceUrl("https://hub.example/_evidence/r/ev1/main?sig=ab")).toBe("/api/evidence/r/ev1/main?sig=ab");
    expect(evidenceUrl("/_evidence/x")).toBe("/api/evidence/x");
  });

  it("never produces an address on another origin", () => {
    expect(evidenceUrl("")).toBe("");
    expect(evidenceUrl(undefined)).toBe("");
    expect(evidenceUrl("https://evil.example/pixel.png")).toBe("");
    expect(evidenceUrl("//evil.example/_evidence/../secret")).toBe("");
    expect(evidenceUrl("/_evidence/")).toBe("");
    expect(evidenceUrl("/_evidence/../../etc/passwd")).toBe("");
    expect(evidenceUrl("/_evidence/a?exp=1", "sha256:nothex")).toBe("/api/evidence/a?exp=1");
  });

  it("replaces a digest the server's URL happened to carry", () => {
    expect(evidenceUrl(`/_evidence/a?${DIGEST_PARAM}=forged`, "sha256:" + HEX)).toBe(`/api/evidence/a?${DIGEST_PARAM}=${HEX}`);
  });

  it("turns a route request back into the upstream URL without the digest", () => {
    expect(parseEvidenceRequest(["r", "ev1", "main"], `?exp=9&sig=ab&${DIGEST_PARAM}=${HEX}`)).toEqual({ upstream: "/_evidence/r/ev1/main?exp=9&sig=ab", digest: HEX });
    expect(parseEvidenceRequest(["a"], "")).toEqual({ upstream: "/_evidence/a", digest: undefined });
    expect(parseEvidenceRequest(["a"], `?${DIGEST_PARAM}=short`)?.digest).toBeUndefined();
  });

  it("refuses paths that could climb out of /_evidence/", () => {
    expect(parseEvidenceRequest([], "")).toBeUndefined();
    expect(parseEvidenceRequest(["a", "..", "b"], "")).toBeUndefined();
    expect(parseEvidenceRequest(["a", ""], "")).toBeUndefined();
    expect(parseEvidenceRequest(["a/b"], "")).toBeUndefined();
    expect(parseEvidenceRequest(["a\\b"], "")).toBeUndefined();
  });

  it("is what the mapping layer hands to screens", async () => {
    const store = new FixtureStore({ anchor: Date.UTC(2026, 9, 18, 9), clock: () => 0, live: false });
    const api = createClient(fixtureRpc(store, "mara-token-0001"));
    const repo = { fullName: "acme/api" };
    const cr = (await api.change.listChanges({ repo })).changes.find((c) => c.title === "Cache invoice totals per customer");
    const items = (await api.brief.listEvidence({ repo, changeIds: [cr?.id ?? ""] })).evidence;
    const image = items.flatMap((e) => e.assets)[0];
    expect(image?.url.startsWith("/api/evidence/fixture/")).toBe(true);
    expect(image?.url).toContain(`${DIGEST_PARAM}=${image?.digest.slice("sha256:".length)}`);
  });
});

describe("ranges", () => {
  it("parses one range and answers the rest in full", () => {
    expect(parseRange(null, 100)).toBe("none");
    expect(parseRange("bytes=0-9", 100)).toEqual([0, 9]);
    expect(parseRange("bytes=90-", 100)).toEqual([90, 99]);
    expect(parseRange("bytes=-10", 100)).toEqual([90, 99]);
    expect(parseRange("bytes=50-500", 100)).toEqual([50, 99]);
    expect(parseRange("bytes=100-", 100)).toBe("invalid");
    expect(parseRange("bytes=9-3", 100)).toBe("invalid");
    expect(parseRange("bytes=-0", 100)).toBe("invalid");
    expect(parseRange("bytes=0-1,5-9", 100)).toBe("none");
    expect(parseRange("items=0-1", 100)).toBe("none");
    expect(parseRange("bytes=-", 100)).toBe("none");
  });
});

describe("serving bytes", () => {
  const bytes = new Uint8Array(Array.from({ length: 100 }, (_, i) => i));
  const get = (headers: Record<string, string> = {}, method = "GET") => new Request("http://x.test/api/evidence/a", { method, headers });
  const image = { contentType: "image/png", bytes, digest: "sha256:" + HEX };

  it("keys the cache on the digest, forbids sniffing and sandboxes the response", async () => {
    const res = serveBytes(get(), image);
    expect(res.status).toBe(200);
    expect(res.headers.get("ETag")).toBe(`"sha256-${HEX}"`);
    expect(res.headers.get("Cache-Control")).toBe("private, max-age=31536000, immutable");
    expect(res.headers.get("X-Content-Type-Options")).toBe("nosniff");
    expect(res.headers.get("Content-Security-Policy")).toBe("default-src 'none'; sandbox");
    expect(res.headers.get("Content-Disposition")).toBeNull();
    expect(res.headers.get("Accept-Ranges")).toBe("bytes");
    expect(new Uint8Array(await res.arrayBuffer())).toEqual(bytes);
  });

  it("answers a range with 206 and the right slice, and a bad one with 416", async () => {
    const part = serveBytes(get({ range: "bytes=10-19" }), image);
    expect(part.status).toBe(206);
    expect(part.headers.get("Content-Range")).toBe("bytes 10-19/100");
    expect(part.headers.get("Content-Length")).toBe("10");
    expect(Array.from(new Uint8Array(await part.arrayBuffer()))).toEqual([10, 11, 12, 13, 14, 15, 16, 17, 18, 19]);
    const bad = serveBytes(get({ range: "bytes=500-" }), image);
    expect(bad.status).toBe(416);
    expect(bad.headers.get("Content-Range")).toBe("bytes */100");
  });

  it("answers 304 when the browser already holds the digest", () => {
    const res = serveBytes(get({ "if-none-match": etagOf(HEX) as string }), image);
    expect(res.status).toBe(304);
    expect(res.headers.get("ETag")).toBe(etagOf(HEX));
    expect(serveBytes(get({ "if-none-match": '"sha256-other"' }), image).status).toBe(200);
  });

  it("makes anything but an image, video or audio an attachment, and caches nothing without a digest", () => {
    const res = serveBytes(get(), { contentType: "text/vtt", bytes });
    expect(res.headers.get("Content-Disposition")).toBe("attachment");
    expect(res.headers.get("Cache-Control")).toBe("private, no-store");
    expect(res.headers.get("ETag")).toBeNull();
    expect(serveBytes(get(), { contentType: "video/mp4", bytes }).headers.get("Content-Disposition")).toBeNull();
  });

  it("sends no body for HEAD", async () => {
    const res = serveBytes(get({}, "HEAD"), image);
    expect(res.status).toBe(200);
    expect(res.headers.get("Content-Length")).toBe("100");
    expect(await res.text()).toBe("");
  });
});

describe("relaying a signed server URL", () => {
  const url = "http://127.0.0.1:7420/_evidence/r/ev/main?sig=1";
  const upstream = (status: number, body: BodyInit | null, headers: Record<string, string> = {}) => async () => new Response(body, { status, headers });
  const get = (headers: Record<string, string> = {}) => new Request("http://x.test/api/evidence/r/ev/main", { headers });

  it("sends the token and the range, and keeps the server's type, length and range", async () => {
    let seen: { url: string; headers: Headers } | undefined;
    const res = await relay(get({ range: "bytes=0-3" }), url, {
      token: "tok",
      digest: HEX,
      fetch: (async (u: string, init: RequestInit) => {
        seen = { url: u, headers: new Headers(init.headers) };
        return new Response("abcd", { status: 206, headers: { "content-type": "image/png", "content-range": "bytes 0-3/10", "content-length": "4" } });
      }) as never,
    });
    expect(seen?.url).toBe(url);
    expect(seen?.headers.get("authorization")).toBe("Bearer tok");
    expect(seen?.headers.get("range")).toBe("bytes=0-3");
    expect(res.status).toBe(206);
    expect(res.headers.get("Content-Range")).toBe("bytes 0-3/10");
    expect(res.headers.get("ETag")).toBe(etagOf(HEX));
    expect(await res.text()).toBe("abcd");
  });

  it("forces nosniff, the sandbox policy and attachment whatever the server sent", async () => {
    const res = await relay(get(), url, { fetch: upstream(200, "<html>", { "content-type": "text/html", "content-disposition": "inline" }) as never });
    expect(res.headers.get("X-Content-Type-Options")).toBe("nosniff");
    expect(res.headers.get("Content-Security-Policy")).toBe("default-src 'none'; sandbox");
    expect(res.headers.get("Content-Disposition")).toBe("attachment");
    expect(res.headers.get("Cache-Control")).toBe("private, no-store");
  });

  it("answers 304 only after the server has said yes", async () => {
    const ok = await relay(get({ "if-none-match": etagOf(HEX) as string }), url, { digest: HEX, fetch: upstream(200, "abcd", { "content-type": "image/png" }) as never });
    expect(ok.status).toBe(304);
    const no = await relay(get({ "if-none-match": etagOf(HEX) as string }), url, { digest: HEX, fetch: upstream(404, "gone") as never });
    expect(no.status).toBe(404);
  });

  it("turns the server's refusals into Connect errors that are never cached", async () => {
    for (const [status, code, reason] of [
      [401, "unauthenticated", "UNAUTHENTICATED"],
      [403, "permission_denied", "PERMISSION_DENIED"],
      [404, "not_found", "EVIDENCE_NOT_FOUND"],
    ] as const) {
      const res = await relay(get(), url, { fetch: upstream(status, "no") as never });
      expect(res.status).toBe(status);
      expect(res.headers.get("Cache-Control")).toBe("no-store");
      expect(await res.json()).toMatchObject({ code, details: [{ debug: { reason } }] });
    }
  });

  it("reports an unreachable server as unavailable", async () => {
    const res = await relay(get(), url, {
      fetch: (async () => {
        throw new Error("connect ECONNREFUSED");
      }) as never,
    });
    expect(res.status).toBe(503);
    expect(await res.json()).toMatchObject({ code: "unavailable" });
  });

  it("does not follow redirects away from the server", async () => {
    let init: RequestInit | undefined;
    await relay(get(), url, {
      fetch: (async (_u: string, i: RequestInit) => {
        init = i;
        return new Response("x", { status: 200 });
      }) as never,
    });
    expect(init?.redirect).toBe("error");
  });
});

describe("relaying an upload", () => {
  const url = "http://127.0.0.1:7420/_evidence/r/ev/main?sig=1";
  const put = (body: string, headers: Record<string, string> = {}) => new Request("http://x.test/api/evidence/r/ev/main", { method: "PUT", body, headers: { "content-type": "image/png", ...headers }, duplex: "half" } as RequestInit);

  it("forwards the body, the type and the token, and answers 204", async () => {
    let seen: { method?: string; headers: Headers } | undefined;
    const res = await relayUpload(put("bytes", { "x-upload-token": "t1", cookie: "secret=1" }), url, {
      token: "tok",
      fetch: (async (_u: string, init: RequestInit) => {
        seen = { method: init.method, headers: new Headers(init.headers) };
        return new Response(null, { status: 200 });
      }) as never,
    });
    expect(res.status).toBe(204);
    expect(seen?.method).toBe("PUT");
    expect(seen?.headers.get("authorization")).toBe("Bearer tok");
    expect(seen?.headers.get("content-type")).toBe("image/png");
    expect(seen?.headers.get("x-upload-token")).toBe("t1");
    expect(seen?.headers.get("cookie")).toBeNull();
  });

  it("passes the server's error body through and refuses oversize uploads before sending", async () => {
    const bad = await relayUpload(put("x"), url, { fetch: (async () => Response.json({ code: "data_loss" }, { status: 500 })) as never });
    expect(bad.status).toBe(500);
    expect(await bad.json()).toEqual({ code: "data_loss" });
    let called = false;
    const big = await relayUpload(put("x", { "content-length": String(MAX_UPLOAD_BYTES + 1) }), url, {
      fetch: (async () => {
        called = true;
        return new Response(null);
      }) as never,
    });
    expect(big.status).toBe(429);
    expect(called).toBe(false);
  });
});

describe("generated PNGs", () => {
  it("are valid and the same every time", () => {
    const c = () => {
      const k = new Canvas(8, 4, [10, 20, 30]);
      k.rect(2, 1, 3, 2, [200, 100, 50]);
      return encodePng(k);
    };
    const a = c();
    expect(Array.from(a.subarray(0, 8))).toEqual([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    expect(new DataView(a.buffer, a.byteOffset).getUint32(16)).toBe(8);
    expect(new DataView(a.buffer, a.byteOffset).getUint32(20)).toBe(4);
    expect(Buffer.from(a).equals(Buffer.from(c()))).toBe(true);
  });
});
