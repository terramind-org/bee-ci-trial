import type { Principal, PrincipalKind } from "./api/types";
/** Pure formatting helpers. Locale is fixed to en-US so server and client agree. */

export function relativeTime(iso: string | undefined, now: number): string {
  if (!iso) return "";
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return "";
  const s = Math.round((now - t) / 1000);
  const future = s < 0;
  const a = Math.abs(s);
  let out: string;
  if (a < 45) out = future ? "in a moment" : "just now";
  else if (a < 3600) out = Math.round(a / 60) + "m";
  else if (a < 86_400) out = Math.round(a / 3600) + "h";
  else if (a < 86_400 * 30) out = Math.round(a / 86_400) + "d";
  else if (a < 86_400 * 365) out = Math.round(a / (86_400 * 30)) + "mo";
  else out = Math.round(a / (86_400 * 365)) + "y";
  if (out.endsWith("now") || out.startsWith("in a")) return out;
  return future ? "in " + out : out + " ago";
}

export function absoluteTime(iso: string | undefined): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return d.toISOString().replace("T", " ").slice(0, 16) + " UTC";
}

export function bytes(n: number | string): string {
  const v = Number(n);
  if (!Number.isFinite(v)) return "";
  if (v < 1024) return v + " B";
  const units = ["KB", "MB", "GB", "TB"];
  let x = v / 1024;
  let i = 0;
  while (x >= 1024 && i < units.length - 1) {
    x /= 1024;
    i++;
  }
  return (x < 10 ? x.toFixed(1) : Math.round(x).toString()) + " " + units[i];
}

export function usd(micros: number | string): string {
  const v = Number(micros) / 1_000_000;
  if (!Number.isFinite(v)) return "";
  return "$" + (v < 10 ? v.toFixed(2) : v.toFixed(0));
}

export function compact(n: number | string): string {
  const v = Number(n);
  if (!Number.isFinite(v)) return "";
  if (v < 1000) return String(v);
  if (v < 1_000_000) return (v / 1000).toFixed(v < 10_000 ? 1 : 0).replace(/\.0$/, "") + "k";
  return (v / 1_000_000).toFixed(1).replace(/\.0$/, "") + "M";
}

export function duration(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return "";
  const s = Math.round(ms / 1000);
  if (s < 60) return s + "s";
  const m = Math.floor(s / 60);
  if (m < 60) return m + "m " + (s % 60) + "s";
  return Math.floor(m / 60) + "h " + (m % 60) + "m";
}

export function between(a: string | undefined, b: string | undefined): number {
  if (!a || !b) return NaN;
  return Date.parse(b) - Date.parse(a);
}

/** ULIDs are long; the tail is the random part, so it is the distinctive bit. */
export function shortId(id: string): string {
  return id.slice(-6).toLowerCase();
}

export function plural(n: number, one: string, many = one + "s"): string {
  return n + " " + (n === 1 ? one : many);
}

export function initials(display: string): string {
  const parts = display.replace(/[^\p{L}\p{N}\s-]/gu, " ").split(/[\s-]+/).filter(Boolean);
  if (parts.length === 0) return "?";
  if (parts.length === 1) return (parts[0] ?? "?").slice(0, 2).toUpperCase();
  return ((parts[0]?.[0] ?? "") + (parts[1]?.[0] ?? "")).toUpperCase();
}

/** First entry per id, in order (directories merged from members and agents). */
export function uniqueById<T extends { id: string }>(items: T[]): T[] {
  return [...new Map(items.map((x) => [x.id, x])).values()];
}

/** A proto3-JSON principal (fields optional) as the screens' Principal (fields present). */
export function principalOf(p: { id?: string; kind?: string; display?: string; ownerId?: string; runtime?: string; externalIds?: string[] } | undefined): Principal | undefined {
  if (!p?.id) return undefined;
  return { id: p.id, kind: (p.kind ?? "PRINCIPAL_KIND_UNSPECIFIED") as PrincipalKind, display: p.display ?? "", ownerId: p.ownerId ?? "", runtime: p.runtime ?? "", externalIds: p.externalIds ?? [] };
}
