/**
 * The evidence route's logic (web-ui §9), apart from Next so it can be tested: serve bytes with
 * `Range`, or relay a signed server URL, and answer both with the headers that make an asset
 * cacheable by its digest and safe to show.
 *
 * Whatever the source, a response carries `X-Content-Type-Options: nosniff` and a sandboxing
 * `Content-Security-Policy`, and anything that is not an image, video or audio is an attachment.
 * Cached copies are keyed by the asset's sha256 (`ETag`), so the expiry of the signed URL that
 * delivered them never breaks a copy the browser already holds.
 */
import { BeeError, httpStatusOf } from "../api/transport";

const CSP = "default-src 'none'; sandbox";
const IMMUTABLE = "private, max-age=31536000, immutable";

/** Response headers the relay copies from the bee server. */
const RELAYED = ["content-type", "content-length", "content-range", "accept-ranges", "content-disposition", "last-modified"];

const isMedia = (contentType: string | null | undefined) => /^(image|video|audio)\//.test(contentType ?? "");

/** `ETag` for a digest: strong, and the same on every server. */
export const etagOf = (hex: string | undefined): string | undefined => (hex ? `"sha256-${hex}"` : undefined);

function matches(ifNoneMatch: string | null, etag: string | undefined): boolean {
  if (!etag || !ifNoneMatch) return false;
  return ifNoneMatch.split(",").some((t) => t.trim() === "*" || t.trim().replace(/^W\//, "") === etag);
}

function baseHeaders(contentType: string, digest: string | undefined): Headers {
  const h = new Headers();
  h.set("Content-Type", contentType);
  h.set("X-Content-Type-Options", "nosniff");
  h.set("Content-Security-Policy", CSP);
  h.set("Accept-Ranges", "bytes");
  if (!isMedia(contentType)) h.set("Content-Disposition", "attachment");
  const tag = etagOf(digest);
  if (tag) {
    h.set("ETag", tag);
    h.set("Cache-Control", IMMUTABLE);
  } else {
    h.set("Cache-Control", "private, no-store");
  }
  return h;
}

/** An error answered as a Connect JSON body, never cached. */
export function errorResponse(err: unknown): Response {
  const e = err instanceof BeeError ? err : new BeeError("internal", { reason: "INTERNAL", message: String(err) });
  return Response.json(e.toJSON(), { status: httpStatusOf(e.code), headers: { "Cache-Control": "no-store", "X-Content-Type-Options": "nosniff" } });
}

/** A single `bytes=` range of a body of `size` bytes: [start, end] inclusive, "none", or "invalid" (416). */
export function parseRange(header: string | null, size: number): [number, number] | "none" | "invalid" {
  if (!header) return "none";
  const m = /^bytes=(\d*)-(\d*)$/.exec(header.trim());
  if (!m) return "none"; // other units and multi-range requests are answered in full
  const [, a = "", b = ""] = m;
  if (a === "" && b === "") return "none";
  let start: number;
  let end: number;
  if (a === "") {
    const n = Number(b);
    if (n === 0) return "invalid";
    start = Math.max(0, size - n);
    end = size - 1;
  } else {
    start = Number(a);
    end = b === "" ? size - 1 : Math.min(Number(b), size - 1);
  }
  return start >= size || start > end ? "invalid" : [start, end];
}

export interface Served {
  contentType: string;
  bytes: Uint8Array;
  /** `sha256:<hex>` or bare hex. */
  digest?: string;
}

/** Serves bytes held here (the fixture's generated media), honouring `Range` and `If-None-Match`. */
export function serveBytes(req: Request, asset: Served): Response {
  const hex = asset.digest?.replace(/^sha256:/, "");
  const headers = baseHeaders(asset.contentType, hex);
  if (matches(req.headers.get("if-none-match"), etagOf(hex))) return new Response(null, { status: 304, headers });
  const size = asset.bytes.length;
  const range = parseRange(req.headers.get("range"), size);
  const head = req.method === "HEAD";
  if (range === "invalid") {
    headers.set("Content-Range", `bytes */${size}`);
    return new Response(null, { status: 416, headers });
  }
  if (range === "none") {
    headers.set("Content-Length", String(size));
    return new Response(head ? null : (asset.bytes as BodyInit), { status: 200, headers });
  }
  const [start, end] = range;
  headers.set("Content-Range", `bytes ${start}-${end}/${size}`);
  headers.set("Content-Length", String(end - start + 1));
  return new Response(head ? null : (asset.bytes.subarray(start, end + 1) as BodyInit), { status: 206, headers });
}

/**
 * Relays a signed `/_evidence/…` URL of the bee server. The caller's token rides in
 * `Authorization`; `Range` passes through. The server decides again who may read (SPEC §9.6), so
 * a 304 is only answered after it has said yes.
 */
export async function relay(req: Request, upstreamUrl: string, opts: { token?: string; digest?: string; fetch?: typeof fetch }): Promise<Response> {
  const headers = new Headers();
  if (opts.token) headers.set("Authorization", "Bearer " + opts.token);
  const range = req.headers.get("range");
  if (range) headers.set("Range", range);
  const doFetch = opts.fetch ?? fetch;
  let res: Response;
  try {
    res = await doFetch(upstreamUrl, { method: req.method === "HEAD" ? "HEAD" : "GET", headers, cache: "no-store", redirect: "error", signal: req.signal });
  } catch (err) {
    if (req.signal.aborted) return new Response(null, { status: 499 });
    return errorResponse(new BeeError("unavailable", { reason: "UNAVAILABLE", message: "cannot reach the bee server: " + (err instanceof Error ? err.message : String(err)), retryable: true }));
  }
  if (res.status !== 200 && res.status !== 206) {
    await res.body?.cancel();
    const code = res.status === 401 ? "unauthenticated" : res.status === 403 ? "permission_denied" : res.status === 404 ? "not_found" : res.status === 416 ? "out_of_range" : "unavailable";
    return errorResponse(new BeeError(code, { reason: res.status === 404 ? "EVIDENCE_NOT_FOUND" : code.toUpperCase(), message: "the bee server answered " + res.status }));
  }
  const contentType = res.headers.get("content-type") ?? "application/octet-stream";
  const out = baseHeaders(contentType, opts.digest);
  for (const name of RELAYED) {
    const v = res.headers.get(name);
    if (v && name !== "content-type") out.set(name, v);
  }
  // A response that is not an image, video or audio never renders inline, whatever the server said.
  if (!isMedia(contentType)) out.set("Content-Disposition", "attachment");
  if (res.status === 200 && matches(req.headers.get("if-none-match"), etagOf(opts.digest))) {
    await res.body?.cancel();
    out.delete("Content-Length");
    return new Response(null, { status: 304, headers: out });
  }
  return new Response(req.method === "HEAD" ? null : res.body, { status: res.status, headers: out });
}

/** The largest upload the route will forward: the biggest per-kind limit (SPEC §11.9, video 100 MiB). */
export const MAX_UPLOAD_BYTES = 100 * 1024 * 1024;

/** Request headers a signed upload may need to repeat. */
const UPLOAD_HEADERS = ["content-type", "content-length", "content-md5"];

/** Forwards a PUT to a signed upload URL; the server checks the signature and the bytes. */
export async function relayUpload(req: Request, upstreamUrl: string, opts: { token?: string; fetch?: typeof fetch }): Promise<Response> {
  const length = Number(req.headers.get("content-length") ?? "0");
  if (length > MAX_UPLOAD_BYTES) return errorResponse(new BeeError("resource_exhausted", { reason: "TOO_LARGE", message: "that file is over the 100 MiB limit" }));
  const headers = new Headers();
  for (const name of UPLOAD_HEADERS) {
    const v = req.headers.get(name);
    if (v) headers.set(name, v);
  }
  for (const [k, v] of req.headers) if (k.startsWith("x-") && !k.startsWith("x-forwarded") && !k.startsWith("x-middleware") && !k.startsWith("x-nextjs")) headers.set(k, v);
  if (opts.token) headers.set("Authorization", "Bearer " + opts.token);
  const doFetch = opts.fetch ?? fetch;
  let res: Response;
  try {
    res = await doFetch(upstreamUrl, { method: "PUT", headers, body: req.body, cache: "no-store", redirect: "error", signal: req.signal, duplex: "half" } as RequestInit);
  } catch (err) {
    if (req.signal.aborted) return new Response(null, { status: 499 });
    return errorResponse(new BeeError("unavailable", { reason: "UNAVAILABLE", message: "cannot reach the bee server: " + (err instanceof Error ? err.message : String(err)), retryable: true }));
  }
  if (res.ok) {
    await res.body?.cancel();
    return new Response(null, { status: 204, headers: { "Cache-Control": "no-store" } });
  }
  // Pass the server's own error body through; it carries the SPEC §12.2 reason.
  return new Response(res.body, { status: res.status, headers: { "Content-Type": res.headers.get("content-type") ?? "application/json", "Cache-Control": "no-store", "X-Content-Type-Options": "nosniff" } });
}
