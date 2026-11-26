/**
 * The mode for this request (lib/mode.ts), read once: who is signed in, `?mode=` from the path
 * the proxy passes on, and the preview cookie. The root layout writes it to `<html data-mode>`;
 * the shell uses it for the agent's header and the switch.
 */
import { cookies, headers } from "next/headers";
import { cache } from "react";
import { sessionState } from "@/lib/api/server";
import type { PrincipalKind } from "@/lib/api/types";
import { MODE_COOKIE, modeParam, resolveMode, type Mode } from "@/lib/mode";
import { PATH_HEADER } from "@/lib/routes";

export const requestMode = cache(async (): Promise<{ mode: Mode; kind: PrincipalKind | null }> => {
  // The document must render even when the session cannot be read: the page below shows the
  // error in its own boundary, and the root layout has none above it.
  const [session, jar, head] = await Promise.all([sessionState().catch(() => null), cookies(), headers()]);
  const kind = session?.me?.kind ?? null;
  return { mode: resolveMode({ kind, param: modeParam(head.get(PATH_HEADER)), cookie: jar.get(MODE_COOKIE)?.value }), kind };
});
