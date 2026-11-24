import { ATOM_TYPE, buildAtom, originOf } from "@/lib/atom";
import { ownerFeed } from "@/lib/feeds";
import { isReservedOwner } from "@/lib/routes";

/** `/{owner}/feed.atom`: recent lands on a user's, agent's or org's public repos. Always anonymous. */
export async function GET(req: Request, { params }: { params: Promise<{ owner: string }> }) {
  const owner = decodeURIComponent((await params).owner);
  const feed = isReservedOwner(owner) ? null : await ownerFeed(originOf(req.url, req.headers), owner);
  if (!feed) return new Response("Not found\n", { status: 404, headers: { "content-type": "text/plain; charset=utf-8" } });
  return new Response(buildAtom(feed), { headers: { "content-type": ATOM_TYPE + "; charset=utf-8", "cache-control": "public, max-age=300" } });
}
