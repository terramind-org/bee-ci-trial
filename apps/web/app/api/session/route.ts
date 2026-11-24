/**
 * POST /api/session {token}: verifies the token with AuthService.WhoAmI and stores it in an
 * httpOnly cookie. DELETE signs out. The token never reaches client-side JavaScript afterwards.
 */
import { cookies } from "next/headers";
import { createClient } from "@/lib/api/client";
import { TOKEN_COOKIE } from "@/lib/api/config";
import { rpcFor } from "@/lib/api/server";
import { BeeError, httpStatusOf } from "@/lib/api/transport";

export const dynamic = "force-dynamic";

export async function POST(req: Request): Promise<Response> {
  const body = (await req.json().catch(() => ({}))) as { token?: unknown };
  const token = typeof body.token === "string" ? body.token.trim() : "";
  if (!token) {
    const e = new BeeError("invalid_argument", { reason: "TOKEN_REQUIRED", message: "paste a token" });
    return Response.json(e.toJSON(), { status: 400 });
  }
  try {
    const who = await createClient(rpcFor(token)).auth.whoAmI({});
    (await cookies()).set(TOKEN_COOKIE, token, {
      httpOnly: true,
      sameSite: "strict",
      // Secure whenever the UI itself is served over HTTPS (directly or behind a proxy).
      secure: new URL(req.url).protocol === "https:" || req.headers.get("x-forwarded-proto") === "https",
      path: "/",
      maxAge: 60 * 60 * 24 * 30,
    });
    return Response.json(who);
  } catch (err) {
    const e = err instanceof BeeError ? err : new BeeError("internal", { reason: "INTERNAL", message: String(err) });
    return Response.json(e.toJSON(), { status: httpStatusOf(e.code) });
  }
}

export async function DELETE(): Promise<Response> {
  (await cookies()).delete(TOKEN_COOKIE);
  return new Response(null, { status: 204 });
}
