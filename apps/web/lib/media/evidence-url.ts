/**
 * Evidence media is same-origin (web-ui §9): the server hands out signed `/_evidence/…` URLs and the
 * mapping rewrites each to `/api/evidence/…`, which the route handler proxies. Pure string code, so
 * the browser bundle, the mapping layer and the route share one definition of the rewrite.
 */

export const EVIDENCE_ROUTE = "/api/evidence/";
const SERVER_PREFIX = "/_evidence/";
/** Query parameter that carries the asset's digest through the proxy; stripped before the upstream call. */
export const DIGEST_PARAM = "_d";

const DIGEST = /^sha256:([0-9a-f]{64})$/;

/** The hex of a `sha256:<hex>` digest; undefined for anything else. */
export function digestHex(digest: string | undefined): string | undefined {
  return DIGEST.exec(digest ?? "")?.[1];
}

/**
 * The same-origin URL for a signed server URL (absolute or path-only). Anything that is not an
 * `/_evidence/` URL maps to "" so a screen can never be handed a remote address to load.
 */
export function evidenceUrl(signed: string | undefined, digest?: string): string {
  if (!signed) return "";
  let path: string;
  let search: string;
  try {
    const u = new URL(signed, "http://bee.invalid");
    path = u.pathname;
    search = u.search;
  } catch {
    return "";
  }
  if (!path.startsWith(SERVER_PREFIX) || path.length === SERVER_PREFIX.length || path.split("/").some((seg) => seg === "." || seg === "..")) return "";
  const params = new URLSearchParams(search);
  params.delete(DIGEST_PARAM);
  const hex = digestHex(digest);
  if (hex) params.set(DIGEST_PARAM, hex);
  const q = params.toString();
  return EVIDENCE_ROUTE + path.slice(SERVER_PREFIX.length) + (q ? "?" + q : "");
}

/** What the route handler needs from a request path: the upstream `/_evidence/…` URL (without the digest) and the digest. */
export function parseEvidenceRequest(segments: string[], search: string): { upstream: string; digest?: string } | undefined {
  if (segments.length === 0 || segments.some((s) => s === "" || s === "." || s === ".." || s.includes("/") || s.includes("\\"))) return undefined;
  const params = new URLSearchParams(search);
  const hex = params.get(DIGEST_PARAM) ?? undefined;
  params.delete(DIGEST_PARAM);
  const q = params.toString();
  return { upstream: SERVER_PREFIX + segments.map(encodeURIComponent).join("/") + (q ? "?" + q : ""), digest: hex && /^[0-9a-f]{64}$/.test(hex) ? hex : undefined };
}
