/**
 * `GET /{owner}/{repo}/changes/{id}/change.md`: the change request as one deterministic Markdown
 * document for agents (docs/spec/web-ui.md §3 "For agents", `lib/changemd.ts`). It accepts
 * `Authorization: Bearer <token>` as well as the session cookie, so an agent fetches it directly
 * with the token it already holds. Errors answer with their SPEC §12.2 reason as JSON.
 */
import { createClient } from "@/lib/api/client";
import { rpcFor, sessionToken } from "@/lib/api/server";
import { BeeError, httpStatusOf } from "@/lib/api/transport";
import { originOf } from "@/lib/atom";
import { bearerToken } from "@/lib/change-page";
import { renderChangeMd } from "@/lib/changemd";
import { isRepoFullName } from "@/lib/routes";
import { changeMdInput } from "../data";

export const dynamic = "force-dynamic";

const PRIVATE = { "cache-control": "private, no-store", vary: "authorization, cookie" };

export async function GET(req: Request, { params }: { params: Promise<{ owner: string; repo: string; id: string }> }): Promise<Response> {
  const { owner, repo, id } = await params;
  const fullName = decodeURIComponent(owner) + "/" + decodeURIComponent(repo);
  if (!isRepoFullName(fullName)) {
    const e = new BeeError("not_found", { reason: "NOT_FOUND", message: "no repository " + fullName });
    return Response.json(e.toJSON(), { status: 404, headers: PRIVATE });
  }
  const token = bearerToken(req.headers.get("authorization")) ?? (await sessionToken());
  try {
    const input = await changeMdInput(createClient(rpcFor(token)), fullName, decodeURIComponent(id), originOf(req.url, req.headers));
    return new Response(renderChangeMd(input), { headers: { "content-type": "text/markdown; charset=utf-8", ...PRIVATE } });
  } catch (err) {
    if (err instanceof BeeError) return Response.json(err.toJSON(), { status: httpStatusOf(err.code), headers: PRIVATE });
    throw err;
  }
}
