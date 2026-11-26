/**
 * Atom 1.0 feeds. `buildAtom` is a pure function of its input: the same entries always give the
 * same bytes, so a feed reader never sees a change that did not happen. Ids are tag URIs,
 * `tag:<host>,2026:<kind>/<id>`, which stay stable when the site moves behind another proxy path.
 */
import type { Metadata } from "next";

export interface AtomEntry {
  /** What the entry is: `land`, `release`. Part of its id. */
  kind: string;
  /** Unique within the kind (a changeset or release id). */
  id: string;
  title: string;
  /** RFC 3339. */
  updated: string;
  /** Where the entry lives, as an absolute path on the site (`/acme/api/activity`). */
  path: string;
  summary?: string;
  author?: string;
}

export interface AtomFeed {
  /** `https://host`, no trailing slash. */
  origin: string;
  /** The feed's own path (`/acme/api/feed.atom`). */
  path: string;
  /** The page the feed describes (`/acme/api`). */
  pagePath: string;
  title: string;
  subtitle?: string;
  entries: AtomEntry[];
}

export const ATOM_TYPE = "application/atom+xml";

/** Feeds without entries still need an `updated`; the epoch says "nothing has happened". */
const EMPTY_UPDATED = "1970-01-01T00:00:00Z";

/** Escapes text and attribute values, and drops the characters XML 1.0 cannot carry at all. */
export function escapeXml(s: string): string {
  return s
    .replace(/[^\u0009\u000A\u000D -퟿-�\u{10000}-\u{10FFFF}]/gu, "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&apos;");
}

/** The id of an entry or feed: `tag:<host>,2026:<kind>/<id>`. */
export function tagId(host: string, kind: string, id: string): string {
  return "tag:" + host + ",2026:" + kind + "/" + id;
}

/** `https://host` for a request: the proxy's forwarded headers win over the URL the server saw. */
export function originOf(url: string, headers: { get(name: string): string | null }): string {
  const u = new URL(url);
  const host = headers.get("x-forwarded-host") ?? headers.get("host") ?? u.host;
  const proto = headers.get("x-forwarded-proto") ?? u.protocol.replace(":", "");
  return proto + "://" + host;
}

function stamp(iso: string): number {
  const t = Date.parse(iso);
  return Number.isFinite(t) ? t : 0;
}

/** RFC 3339 in UTC, which is what Atom wants; an unreadable time becomes the epoch. */
export function rfc3339(iso: string): string {
  const t = stamp(iso);
  return new Date(t).toISOString().replace(/\.\d{3}Z$/, "Z");
}

export function buildAtom(feed: AtomFeed): string {
  const host = new URL(feed.origin).host;
  const entries = [...feed.entries].sort((a, b) => stamp(b.updated) - stamp(a.updated) || tagId(host, a.kind, a.id).localeCompare(tagId(host, b.kind, b.id)));
  const updated = entries[0] ? rfc3339(entries[0].updated) : EMPTY_UPDATED;
  const out: string[] = ['<?xml version="1.0" encoding="utf-8"?>', '<feed xmlns="http://www.w3.org/2005/Atom">'];
  out.push("  <title>" + escapeXml(feed.title) + "</title>");
  if (feed.subtitle) out.push("  <subtitle>" + escapeXml(feed.subtitle) + "</subtitle>");
  out.push("  <id>" + escapeXml(tagId(host, "feed", feed.pagePath.replace(/^\/+/, ""))) + "</id>");
  out.push("  <updated>" + updated + "</updated>");
  out.push('  <link rel="self" type="' + ATOM_TYPE + '" href="' + escapeXml(feed.origin + feed.path) + '"/>');
  out.push('  <link rel="alternate" type="text/html" href="' + escapeXml(feed.origin + feed.pagePath) + '"/>');
  out.push("  <generator>Beehive</generator>");
  for (const e of entries) {
    out.push("  <entry>");
    out.push("    <title>" + escapeXml(e.title) + "</title>");
    out.push("    <id>" + escapeXml(tagId(host, e.kind, e.id)) + "</id>");
    out.push("    <updated>" + rfc3339(e.updated) + "</updated>");
    out.push('    <link rel="alternate" type="text/html" href="' + escapeXml(feed.origin + e.path) + '"/>');
    if (e.author) out.push("    <author><name>" + escapeXml(e.author) + "</name></author>");
    if (e.summary) out.push('    <summary type="text">' + escapeXml(e.summary) + "</summary>");
    out.push("  </entry>");
  }
  out.push("</feed>");
  return out.join("\n") + "\n";
}

/**
 * `generateMetadata().alternates` for a page that has a feed: `feedAlternates("/acme/api")` makes
 * the browser and feed readers offer `/acme/api/feed.atom`.
 */
export function feedAlternates(pagePath: string, title?: string): NonNullable<Metadata["alternates"]> {
  const base = pagePath.replace(/\/+$/, "");
  return { types: { [ATOM_TYPE]: [{ url: base + "/feed.atom", title: title ?? "Activity" }] } };
}
