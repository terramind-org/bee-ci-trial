"use server";

/**
 * Server action behind the source archive buttons: ReleaseService.GetSourceArchiveUrl as the
 * session's principal (`sourceArchiveUrl` in lib/api/server.ts). It answers with a result, never
 * throws, so the panel can print the server's own refusal.
 */
import { sourceArchiveUrl } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import { isRepoFullName } from "@/lib/routes";

export type ArchiveResult = { ok: true; url: string } | { ok: false; reason: string; message: string };

export async function requestArchive(fullName: string, format: "tar.gz" | "zip"): Promise<ArchiveResult> {
  if (!isRepoFullName(fullName)) return { ok: false, reason: "INVALID_ARGUMENT", message: "not a repository name: " + fullName };
  try {
    return { ok: true, url: await sourceArchiveUrl(fullName, format) };
  } catch (err) {
    if (err instanceof BeeError) return { ok: false, reason: err.reason, message: err.message };
    return { ok: false, reason: "INTERNAL", message: err instanceof Error ? err.message : String(err) };
  }
}
