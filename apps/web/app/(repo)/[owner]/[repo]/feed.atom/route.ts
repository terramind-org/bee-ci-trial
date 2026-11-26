import { ATOM_TYPE, buildAtom, originOf } from "@/lib/atom";
import { repoFeed } from "@/lib/feeds";
import { isRepoFullName } from "@/lib/routes";

/** `/{owner}/{repo}/feed.atom`: lands, releases and posts of a public repo. Always anonymous. */
export async function GET(req: Request, { params }: { params: Promise<{ owner: string; repo: string }> }) {
  const { owner, repo } = await params;
  const fullName = decodeURIComponent(owner) + "/" + decodeURIComponent(repo);
  const feed = isRepoFullName(fullName) ? await repoFeed(originOf(req.url, req.headers), fullName) : null;
  if (!feed) return new Response("Not found\n", { status: 404, headers: { "content-type": "text/plain; charset=utf-8" } });
  return new Response(buildAtom(feed), { headers: { "content-type": ATOM_TYPE + "; charset=utf-8", "cache-control": "public, max-age=300" } });
}
