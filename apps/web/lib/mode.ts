/**
 * The two ways the interface presents itself (docs/spec/web-ui.md §2, "Two modes"):
 *
 *   keeper  for a person: sentences, decisions, pictures.
 *   bee     for an agent: the same facts as plain mono text, no motion, and the page's actions
 *           as tools.
 *
 * The mode is chosen on the server from who is signed in and written to `<html data-mode>`, so
 * the first byte is already right. An agent always gets `bee`. A person gets `keeper`, and may
 * preview the agent's view with `?mode=bee` or the cookie the rail's switch sets.
 */
import type { PrincipalKind } from "./api/types";

export type Mode = "keeper" | "bee";

export const MODES: readonly Mode[] = ["keeper", "bee"];
/** Remembers a person's preview choice. Not a secret, readable by the page's script. */
export const MODE_COOKIE = "bee_mode";
/** `?mode=bee` or `?mode=keeper`. */
export const MODE_PARAM = "mode";
/** A year: a preview choice lasts until the person changes it. */
export const MODE_COOKIE_MAX_AGE = 60 * 60 * 24 * 365;

export function isMode(value: unknown): value is Mode {
  return value === "keeper" || value === "bee";
}

/**
 * The mode for one request. `kind` is the signed-in principal's kind, or null when signed out.
 * An agent cannot leave `bee`: the person's layout is not addressed to it. Anyone else gets what
 * the URL asks for, else what the cookie remembers, else `keeper`. Unknown values are ignored.
 */
export function resolveMode(input: { kind: PrincipalKind | null | undefined; param?: string | null; cookie?: string | null }): Mode {
  if (input.kind === "PRINCIPAL_KIND_AGENT") return "bee";
  if (isMode(input.param)) return input.param;
  if (isMode(input.cookie)) return input.cookie;
  return "keeper";
}

/** Whether the caller may change the mode: anyone but an agent. */
export function canSwitchMode(kind: PrincipalKind | null | undefined): boolean {
  return kind !== "PRINCIPAL_KIND_AGENT";
}

/** `?mode=` from a path with its query (`/acme/api?mode=bee`), or null. */
export function modeParam(pathWithQuery: string | null | undefined): string | null {
  if (!pathWithQuery) return null;
  const q = pathWithQuery.indexOf("?");
  if (q === -1) return null;
  return new URLSearchParams(pathWithQuery.slice(q + 1)).get(MODE_PARAM);
}

/** The `Set-Cookie` value (for `document.cookie`) that remembers a preview choice. */
export function modeCookie(mode: Mode): string {
  return MODE_COOKIE + "=" + mode + "; Path=/; Max-Age=" + MODE_COOKIE_MAX_AGE + "; SameSite=Lax";
}
