import { adapterKind, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";

/**
 * `/{owner}/{repo}/posts/{postId}/media/{mediaId}`: one of a post's images or its video.
 *
 * Read access is the API's: the post is read with the visitor's session (PostService.GetPost, which
 * signed-out visitors may call on public repos), so a private repo's media answers 404 to anyone
 * who cannot read the repo. The answer is a 302 to the signed URL the read just minted, so the
 * address on the page never expires while the bytes behind it stay private.
 */
export async function GET(_req: Request, { params }: { params: Promise<{ owner: string; repo: string; postId: string; mediaId: string }> }) {
  const p = await params;
  const fullName = decodeURIComponent(p.owner) + "/" + decodeURIComponent(p.repo);
  const postId = decodeURIComponent(p.postId);
  const mediaId = decodeURIComponent(p.mediaId);
  let post;
  try {
    post = await (await serverClient()).post.getPost({ postId });
  } catch (err) {
    if (err instanceof BeeError && ["not_found", "permission_denied", "unauthenticated", "invalid_argument"].includes(err.code)) return notFound();
    throw err;
  }
  if (post.repo?.fullName !== fullName) return notFound();
  const media = post.media.find((m) => m.id === mediaId);
  if (!media?.url) return notFound();
  const url = media.url;
  if (url.startsWith("data:")) return inline(url);
  // A relative Location (the fixture's own files) resolves against this origin in the browser.
  return new Response(null, { status: 302, headers: { location: await absolute(url), "cache-control": "private, no-store" } });
}

function notFound() {
  return new Response("Not found\n", { status: 404, headers: { "content-type": "text/plain; charset=utf-8", "cache-control": "private, no-store" } });
}

/**
 * A signed URL as the browser can open it. The server signs paths relative to its public address,
 * which only it knows (GetServerInfo.connect_base_url); the fixture world's media are files the
 * web app itself serves, so its paths stay on this origin.
 */
async function absolute(url: string): Promise<string> {
  if (/^https?:\/\//.test(url)) return url;
  if (adapterKind() === "fixture") return url;
  const info = await (await serverClient()).object.getServerInfo({});
  return (info.connectBaseUrl ?? "").replace(/\/+$/, "") + "/" + url.replace(/^\/+/, "");
}

/** Media the fixture world received from a client upload: it holds the bytes as a data URL. */
function inline(url: string): Response {
  const m = /^data:([\w.+-]+\/[\w.+-]+);base64,(.*)$/s.exec(url);
  if (!m) return notFound();
  const bytes = Buffer.from(m[2] ?? "", "base64");
  return new Response(bytes, { headers: { "content-type": m[1] ?? "application/octet-stream", "content-length": String(bytes.length), "x-content-type-options": "nosniff", "cache-control": "private, no-store" } });
}
