/**
 * PUT /api/post-media/<repo-id>/<media-id>?expires=…&signature=…: a post's media upload, same-origin.
 *
 * `BeginPostMediaUpload` answers a signed URL on the bee server (`/posts/<repo-id>/<media-id>`);
 * the composer rewrites it here (`uploadPath` in lib/post-compose.ts) so the browser never sends
 * bytes across origins. The route relays the body and the signature to the server with the
 * session's token, and passes the server's own error, with its SPEC §12.2 reason, back. With
 * BEE_ADAPTER=fixture the demo world takes the bytes and checks them as the server would.
 */
import { acceptFixturePostUpload, adapterKind, apiUrl, sessionToken } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import { errorResponse, relayUpload } from "@/lib/media/evidence-proxy";
import { MAX_VIDEO_BYTES } from "@/lib/post-media";
import { parseUploadSegments } from "@/lib/post-compose";

export const dynamic = "force-dynamic";

export async function PUT(req: Request, ctx: { params: Promise<{ repo: string; media: string }> }): Promise<Response> {
  const { repo, media } = await ctx.params;
  const ids = parseUploadSegments([decodeURIComponent(repo), decodeURIComponent(media)]);
  if (!ids) return errorResponse(new BeeError("not_found", { reason: "MEDIA_NOT_FOUND", message: "no such upload" }));
  const token = await sessionToken();
  const search = new URL(req.url).search;

  if (adapterKind() === "http") {
    return relayUpload(req, apiUrl() + "/posts/" + encodeURIComponent(ids.repoId) + "/" + encodeURIComponent(ids.mediaId) + search, { token });
  }

  try {
    const bytes = new Uint8Array(await req.arrayBuffer());
    if (bytes.length > MAX_VIDEO_BYTES) throw new BeeError("resource_exhausted", { reason: "TOO_LARGE", message: "an image is at most 8 MiB and a video 64 MiB" });
    acceptFixturePostUpload(ids.mediaId, bytes);
    return new Response(null, { status: 204, headers: { "Cache-Control": "no-store" } });
  } catch (err) {
    return errorResponse(err);
  }
}
