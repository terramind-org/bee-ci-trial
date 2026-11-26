"use server";

/**
 * Server action behind the release archive links. It asks `ReleaseService.GetSourceArchiveUrl`
 * through the session's token, as an agent would, and answers with a result instead of throwing so
 * the page can print the server's reason where the link would have been.
 *
 * The call itself is refused with POLICY_DENIED when the caller cannot read every path. A tree that
 * holds redacted content is only refused when the archive is fetched (SPEC §13), so the action
 * checks the signed URL's status before handing it to the browser and turns that refusal into the
 * REDACTED reason instead of a download that fails.
 */
import { serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { SourceArchiveFormat } from "@/lib/api/types";
import type { ArchiveOutcome } from "@/lib/infra";

const NAME = /^[\w.-]+\/[\w.-]+$/;
const FORMATS: ReadonlySet<string> = new Set<SourceArchiveFormat>(["SOURCE_ARCHIVE_FORMAT_TAR_GZ", "SOURCE_ARCHIVE_FORMAT_ZIP"]);

async function refusalOf(url: string): Promise<ArchiveOutcome | null> {
  // Only the status matters, so the request is dropped as soon as the headers arrive and bounded
  // in time: an archive is streamed and may be large.
  const stop = new AbortController();
  const timer = setTimeout(() => stop.abort(), 8000);
  let res: Response;
  try {
    res = await fetch(url, { cache: "no-store", signal: stop.signal });
  } catch {
    clearTimeout(timer);
    return null; // The server cannot reach the URL the browser may: let the browser try.
  }
  if (res.ok) {
    clearTimeout(timer);
    stop.abort();
    return null;
  }
  const text = (await res.text().finally(() => clearTimeout(timer))).slice(0, 400);
  const reason = /^REDACTED\b/.test(text) ? "REDACTED" : res.status === 403 ? "POLICY_DENIED" : res.status === 404 ? "NOT_FOUND" : "UNAVAILABLE";
  return { ok: false, reason, message: text || "The archive could not be fetched (HTTP " + res.status + ")." };
}

export async function requestSourceArchive(fullName: string, at: string, format: SourceArchiveFormat): Promise<ArchiveOutcome> {
  if (!NAME.test(fullName) || !FORMATS.has(format) || !at) return { ok: false, reason: "INVALID_ARGUMENT", message: "not an archive request: " + fullName + " " + at };
  try {
    const res = await (await serverClient()).release.getSourceArchiveUrl({ repo: { fullName }, at: { expr: at }, format });
    return (await refusalOf(res.url)) ?? { ok: true, url: res.url, filtered: res.filtered };
  } catch (err) {
    if (err instanceof BeeError) return { ok: false, reason: err.reason, message: err.message };
    return { ok: false, reason: "INTERNAL", message: err instanceof Error ? err.message : String(err) };
  }
}
