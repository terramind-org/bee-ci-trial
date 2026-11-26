/**
 * Where agents reach this instance, for filling the prompts. Server-only (it reads request
 * headers and calls the API): lib/prompts.ts stays importable from client components.
 */
import { cache } from "react";
import { headers } from "next/headers";
import { ObjectService } from "@/lib/gen/bee/v1/vcs_pb";
import { rpcFor } from "@/lib/api/server";

export interface PromptTargets {
  /** The server's base URL (`ServerInfo.connectBaseUrl`, else the request origin). */
  endpoint: string;
  /** The web app's own origin, for links to its pages. */
  origin: string;
}

const first = (v: string | null) => (v ?? "").split(",")[0]!.trim();

/** The origin this request came in on, honouring a reverse proxy's forwarded headers. */
export async function requestOrigin(): Promise<string> {
  const h = await headers();
  const host = first(h.get("x-forwarded-host")) || first(h.get("host")) || "localhost";
  const proto = first(h.get("x-forwarded-proto")) || (/^(localhost|127\.|\[::1\])/.test(host) ? "http" : "https");
  return `${proto}://${host}`;
}

export const promptTargets = cache(async (): Promise<PromptTargets> => {
  const origin = await requestOrigin();
  try {
    // GetServerInfo is anonymous; the endpoint it reports is the one clients are meant to use.
    const info = await rpcFor(undefined).call(ObjectService.method.getServerInfo, {});
    const base = (info.connectBaseUrl ?? "").trim().replace(/\/+$/, "");
    if (/^https?:\/\//.test(base)) return { endpoint: base, origin };
  } catch {
    // An unreachable server still gets prompts: they fall back to this host.
  }
  return { endpoint: origin, origin };
});
