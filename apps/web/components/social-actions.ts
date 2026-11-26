"use server";

/**
 * Server actions behind the star and follow buttons. They call UserService through the
 * session's token, exactly as an agent would (`bee.v1.UserService/StarRepo` and friends).
 * They answer with a result rather than throwing, so the button can show the server's reason.
 */
import { revalidatePath } from "next/cache";
import { setFollowing, setStar } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import { ownerHref, repoHref } from "@/lib/routes";

export type SocialResult = { ok: true } | { ok: false; reason: string; message: string };

async function run(call: () => Promise<unknown>): Promise<SocialResult> {
  try {
    await call();
    return { ok: true };
  } catch (err) {
    if (err instanceof BeeError) return { ok: false, reason: err.reason, message: err.message };
    return { ok: false, reason: "INTERNAL", message: err instanceof Error ? err.message : String(err) };
  }
}

const NAME = /^[\w.-]+(\/[\w.-]+)?$/;

/** Stars (`on`) or unstars a repo for the signed-in caller. */
export async function setRepoStar(fullName: string, on: boolean): Promise<SocialResult> {
  if (!NAME.test(fullName) || !fullName.includes("/")) return { ok: false, reason: "INVALID_ARGUMENT", message: "not a repository name: " + fullName };
  const res = await run(() => setStar(fullName, on));
  if (res.ok) revalidatePath(repoHref(fullName), "layout");
  return res;
}

/** Follows (`on`) or unfollows a user or agent by login for the signed-in caller. */
export async function setFollow(login: string, on: boolean): Promise<SocialResult> {
  if (!NAME.test(login) || login.includes("/")) return { ok: false, reason: "INVALID_ARGUMENT", message: "not a login: " + login };
  const res = await run(() => setFollowing(login, on));
  if (res.ok) revalidatePath(ownerHref(login));
  return res;
}
