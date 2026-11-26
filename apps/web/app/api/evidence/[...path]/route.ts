/**
 * GET /api/evidence/<path>[?…]: same-origin evidence media (web-ui §9).
 *
 * The browser never fetches evidence from the bee server. `EvidenceAsset.url` is a signed
 * `/_evidence/…` URL; the mapping rewrites it to this route, which fetches the signed URL with the
 * session's token, streams the bytes back with `Range` support and caches them by the asset's
 * digest. PUT carries an evidence upload to the signed URL `BeginEvidenceUpload` returned, the
 * same way. With BEE_ADAPTER=fixture both are served from the demo world instead.
 */
import { acceptFixtureUpload, adapterKind, apiUrl, fixtureEvidence, sessionToken } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import { DIGEST_PARAM, parseEvidenceRequest } from "@/lib/media/evidence-url";
import { MAX_UPLOAD_BYTES, errorResponse, relay, relayUpload, serveBytes } from "@/lib/media/evidence-proxy";

export const dynamic = "force-dynamic";

async function serve(req: Request, ctx: { params: Promise<{ path: string[] }> }): Promise<Response> {
  const { path } = await ctx.params;
  const url = new URL(req.url);
  const parsed = parseEvidenceRequest(path.map((s) => decodeURIComponent(s)), url.search);
  if (!parsed) return errorResponse(new BeeError("not_found", { reason: "EVIDENCE_NOT_FOUND", message: "no such evidence file" }));
  const token = await sessionToken();

  if (adapterKind() === "http") return relay(req, apiUrl() + parsed.upstream, { token, digest: parsed.digest });

  try {
    const params = new URLSearchParams(url.search);
    params.delete(DIGEST_PARAM);
    const asset = fixtureEvidence(token, path.map((s) => decodeURIComponent(s)), params);
    return serveBytes(req, asset);
  } catch (err) {
    return errorResponse(err);
  }
}

async function upload(req: Request, ctx: { params: Promise<{ path: string[] }> }): Promise<Response> {
  const { path } = await ctx.params;
  const url = new URL(req.url);
  const segments = path.map((s) => decodeURIComponent(s));
  const parsed = parseEvidenceRequest(segments, url.search);
  if (!parsed) return errorResponse(new BeeError("not_found", { reason: "EVIDENCE_NOT_FOUND", message: "no such evidence file" }));
  const token = await sessionToken();

  if (adapterKind() === "http") return relayUpload(req, apiUrl() + parsed.upstream, { token });

  try {
    const bytes = new Uint8Array(await req.arrayBuffer());
    if (bytes.length > MAX_UPLOAD_BYTES) throw new BeeError("resource_exhausted", { reason: "TOO_LARGE", message: "that file is over the 100 MiB limit" });
    acceptFixtureUpload(token, segments, new URLSearchParams(url.search), bytes);
    return new Response(null, { status: 204, headers: { "Cache-Control": "no-store" } });
  } catch (err) {
    return errorResponse(err);
  }
}

export const GET = serve;
export const PUT = upload;
export const HEAD = serve;
