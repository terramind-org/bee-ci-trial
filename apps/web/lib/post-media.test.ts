import { createHash } from "node:crypto";
import { describe, expect, it, vi } from "vitest";
import type { BeeClient } from "./api/client";
import type * as T from "./api/types";
import { createClient } from "./api/client";
import { FixtureStore, fixtureRpc } from "./api/fixture";
import { MAX_IMAGE_BYTES, browserDecode, checkUpload, mediaKind, readMediaInfo, sha256Hex, uploadPostMedia, type DecodeEnv } from "./post-media";

const repo = { fullName: "acme/api" };
const bytes = Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 7, 7, 7]);
const png = () => new File([bytes], "shot.png", { type: "image/png" });
const mp4 = () => new File([Uint8Array.from([0, 0, 0, 24, 0x66, 0x74, 0x79, 0x70, 1, 2])], "clip.mp4", { type: "video/mp4" });
const sha = createHash("sha256").update(bytes).digest("hex");

const decode: DecodeEnv = {
  image: async () => ({ width: 1600, height: 1000 }),
  video: async () => ({ width: 1280, height: 800, durationSeconds: 7.9996 }),
};

function fakeClient(over: { finalize?: (r: T.FinalizePostMediaRequest) => T.PostMedia } = {}) {
  const calls: { begin: T.BeginPostMediaUploadRequest[]; finalize: T.FinalizePostMediaRequest[] } = { begin: [], finalize: [] };
  const client = {
    post: {
      beginPostMediaUpload: async (r: T.BeginPostMediaUploadRequest): Promise<T.BeginPostMediaUploadResponse> => {
        calls.begin.push(r);
        return { mediaId: "01MEDIA", uploadUrl: "https://assets.example/posts/repo/01MEDIA?sig=abc", uploadHeaders: { "content-type": r.contentType, "x-upload-token": "t0k" } };
      },
      finalizePostMedia: async (r: T.FinalizePostMediaRequest): Promise<T.PostMedia> => {
        calls.finalize.push(r);
        return over.finalize ? over.finalize(r) : { id: r.mediaId, kind: "POST_MEDIA_KIND_IMAGE", contentType: "image/png", sizeBytes: "11", width: r.width ?? 0, height: r.height ?? 0, durationMs: r.durationMs ?? 0, altText: r.altText ?? "", sha256: r.sha256, url: "" };
      },
    },
  } as unknown as Pick<BeeClient, "post">;
  return { client, calls };
}

const ok = () => vi.fn(async () => new Response(null, { status: 200 }));

describe("post media", () => {
  it("names the kind of a content type and refuses the rest", () => {
    expect(mediaKind("image/webp")).toBe("POST_MEDIA_KIND_IMAGE");
    expect(mediaKind("video/webm")).toBe("POST_MEDIA_KIND_VIDEO");
    for (const t of ["image/svg+xml", "text/html", "application/javascript", "video/quicktime", ""]) expect(mediaKind(t)).toBeUndefined();
  });

  it("hashes with WebCrypto, in lower-case hex", async () => {
    expect(await sha256Hex(png())).toBe(sha);
    expect(await sha256Hex(new Blob([]))).toBe("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
  });

  it("reads image and video dimensions, and a video's length in milliseconds", async () => {
    expect(await readMediaInfo(png(), "POST_MEDIA_KIND_IMAGE", decode)).toEqual({ width: 1600, height: 1000, durationMs: 0 });
    expect(await readMediaInfo(mp4(), "POST_MEDIA_KIND_VIDEO", decode)).toEqual({ width: 1280, height: 800, durationMs: 8000 });
    const live = { ...decode, video: async () => ({ width: 2, height: 2, durationSeconds: Infinity }) };
    expect((await readMediaInfo(mp4(), "POST_MEDIA_KIND_VIDEO", live)).durationMs).toBe(0);
  });

  it("decodes with the browser's bitmap and video APIs", async () => {
    const close = vi.fn();
    vi.stubGlobal("createImageBitmap", async () => ({ width: 30, height: 20, close }));
    expect(await browserDecode.image(png())).toEqual({ width: 30, height: 20 });
    expect(close).toHaveBeenCalled();
    const el: Record<string, unknown> = { videoWidth: 640, videoHeight: 360, duration: 3.5 };
    vi.stubGlobal("document", { createElement: () => el });
    vi.stubGlobal("URL", { createObjectURL: () => "blob:x", revokeObjectURL: vi.fn() });
    const pending = browserDecode.video(mp4());
    (el.onloadedmetadata as () => void)();
    expect(await pending).toEqual({ width: 640, height: 360, durationSeconds: 3.5 });
    const failing = browserDecode.video(mp4());
    (el.onerror as () => void)();
    await expect(failing).rejects.toThrow(/could not read/);
    vi.unstubAllGlobals();
  });

  it("runs Begin, PUT, Finalize in that order and returns the finalized media", async () => {
    const order: string[] = [];
    const fetchMock = vi.fn<typeof fetch>(async () => {
      order.push("put");
      return new Response(null, { status: 200 });
    });
    const { client, calls } = fakeClient({
      finalize: (r) => {
        order.push("finalize");
        return { id: r.mediaId, kind: "POST_MEDIA_KIND_IMAGE", contentType: "image/png", sizeBytes: "11", width: r.width ?? 0, height: r.height ?? 0, durationMs: 0, altText: r.altText ?? "", sha256: r.sha256, url: "" };
      },
    });
    const begin = client.post.beginPostMediaUpload;
    client.post.beginPostMediaUpload = async (r) => {
      order.push("begin");
      return begin(r);
    };
    const media = await uploadPostMedia(client, { repo, changeId: "cr1", file: png(), altText: "  A dark screenshot of the Inbox.  " }, { fetch: fetchMock, decode });
    expect(order).toEqual(["begin", "put", "finalize"]);
    expect(calls.begin).toEqual([{ repo, changeId: "cr1", kind: "POST_MEDIA_KIND_IMAGE", contentType: "image/png", sizeBytes: "11" }]);
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("https://assets.example/posts/repo/01MEDIA?sig=abc");
    expect(init).toMatchObject({ method: "PUT", headers: { "content-type": "image/png", "x-upload-token": "t0k" } });
    expect((init.body as Blob).size).toBe(11);
    expect(calls.finalize).toEqual([{ repo, mediaId: "01MEDIA", sha256: sha, altText: "A dark screenshot of the Inbox.", width: 1600, height: 1000, durationMs: 0 }]);
    expect(media).toMatchObject({ id: "01MEDIA", sha256: sha, altText: "A dark screenshot of the Inbox." });
  });

  it("uploads a video without alt text and sends its duration", async () => {
    const { client, calls } = fakeClient();
    await uploadPostMedia(client, { repo, changeId: "cr1", file: mp4() }, { fetch: ok() as unknown as typeof fetch, decode });
    expect(calls.begin[0]).toMatchObject({ kind: "POST_MEDIA_KIND_VIDEO", contentType: "video/mp4", sizeBytes: "10" });
    expect(calls.finalize[0]).toMatchObject({ altText: "", width: 1280, height: 800, durationMs: 8000 });
  });

  it("refuses an image without alt text, a type a post does not take and a file over the limit, before any call", async () => {
    const { client, calls } = fakeClient();
    const f = ok() as unknown as typeof fetch;
    await expect(uploadPostMedia(client, { repo, changeId: "c", file: png(), altText: "   " }, { fetch: f, decode })).rejects.toMatchObject({ code: "invalid_argument", reason: "alt_text_required" });
    await expect(uploadPostMedia(client, { repo, changeId: "c", file: png(), altText: "x".repeat(1001) }, { fetch: f, decode })).rejects.toMatchObject({ reason: "alt_text_required" });
    await expect(uploadPostMedia(client, { repo, changeId: "c", file: new File(["<svg/>"], "a.svg", { type: "image/svg+xml" }), altText: "x" }, { fetch: f, decode })).rejects.toMatchObject({ reason: "bad_media" });
    const big = { size: MAX_IMAGE_BYTES + 1, type: "image/png" } as Blob & { type: string };
    expect(() => checkUpload(big, "x")).toThrow(/8 MiB/);
    expect(calls.begin).toEqual([]);
    expect(f).not.toHaveBeenCalled();
  });

  it("turns a failed or refused PUT into UPLOAD_FAILED and never finalizes", async () => {
    const { client, calls } = fakeClient();
    const refused = vi.fn(async () => new Response(null, { status: 403 })) as unknown as typeof fetch;
    await expect(uploadPostMedia(client, { repo, changeId: "c", file: png(), altText: "x" }, { fetch: refused, decode })).rejects.toMatchObject({ code: "invalid_argument", reason: "UPLOAD_FAILED" });
    const down = vi.fn(async () => new Response(null, { status: 503 })) as unknown as typeof fetch;
    await expect(uploadPostMedia(client, { repo, changeId: "c", file: png(), altText: "x" }, { fetch: down, decode })).rejects.toMatchObject({ code: "unavailable", reason: "UPLOAD_FAILED", retryable: true });
    const offline = vi.fn(async () => {
      throw new TypeError("network down");
    }) as unknown as typeof fetch;
    await expect(uploadPostMedia(client, { repo, changeId: "c", file: png(), altText: "x" }, { fetch: offline, decode })).rejects.toMatchObject({ code: "unavailable", message: expect.stringContaining("network down") });
    expect(calls.finalize).toEqual([]);
  });

  it("passes the server's Finalize errors through", async () => {
    const { client } = fakeClient({
      finalize: () => {
        throw Object.assign(new Error("bad hash"), { code: "data_loss", reason: "INTEGRITY" });
      },
    });
    await expect(uploadPostMedia(client, { repo, changeId: "c", file: png(), altText: "x" }, { fetch: ok() as unknown as typeof fetch, decode })).rejects.toMatchObject({ reason: "INTEGRITY" });
  });
});

describe("post media against the fixture server", () => {
  it("uploads through the real mapping layer and the fixture checks the bytes against the hash", async () => {
    const store = new FixtureStore({ anchor: Date.UTC(2026, 9, 18, 9, 0, 0), clock: () => 0, live: false });
    const client = createClient(fixtureRpc(store, "mara-token-0001"));
    const cr = (await client.change.listChanges({ repo: { fullName: "acme/infra" } })).changes[0]!;
    // The fixture has no HTTP asset route; this stands in for it by handing the PUT body to the store.
    const put = (async (url: string, init: RequestInit) => {
      store.posts.receive(String(url).split("/").pop()!, new Uint8Array(await (init.body as Blob).arrayBuffer()));
      return new Response(null, { status: 200 });
    }) as unknown as typeof fetch;
    const media = await uploadPostMedia(client, { repo: { fullName: "acme/infra" }, changeId: cr.id, file: png(), altText: "A pixel" }, { fetch: put, decode });
    expect(media).toMatchObject({ sha256: sha, sizeBytes: "11", width: 1600, height: 1000, altText: "A pixel", contentType: "image/png" });
    const post = await client.post.setChangePost({ repo: { fullName: "acme/infra" }, changeId: cr.id, text: "", mediaIds: [media.id] });
    expect(post.media[0]?.url).toMatch(/^data:image\/png;base64,/);
  });
});
