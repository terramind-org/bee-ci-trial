/**
 * Uploading media for a change request's post: Begin, a signed PUT of the bytes, then Finalize
 * with their sha256. The server checks the bytes against the hash, the magic bytes against the
 * kind and (for images) the alt text, so this only has to send them honestly.
 *
 *   const media = await uploadPostMedia(client, { repo, changeId, file, altText });
 *   await client.post.setChangePost({ repo, changeId, text, mediaIds: [media.id] });
 *
 * Browser APIs (fetch, WebCrypto, image and video decoding) are injectable so the flow runs
 * under test and on a server without a DOM.
 */
import type { BeeClient } from "./api/client";
import { BeeError } from "./api/transport";
import type { PostMedia, PostMediaKind, RepoRef } from "./api/types";

export const IMAGE_TYPES = ["image/png", "image/jpeg", "image/gif", "image/webp"] as const;
export const VIDEO_TYPES = ["video/mp4", "video/webm"] as const;
/** The server's limits (design: Rules, Media). */
export const MAX_IMAGE_BYTES = 8 * 1024 * 1024;
export const MAX_VIDEO_BYTES = 64 * 1024 * 1024;
export const MAX_IMAGES = 4;
export const MAX_ALT_CHARS = 1000;

/** What a file is, from its content type; undefined for a type a post does not take. */
export function mediaKind(contentType: string): PostMediaKind | undefined {
  if ((IMAGE_TYPES as readonly string[]).includes(contentType)) return "POST_MEDIA_KIND_IMAGE";
  if ((VIDEO_TYPES as readonly string[]).includes(contentType)) return "POST_MEDIA_KIND_VIDEO";
  return undefined;
}

export interface MediaInfo {
  width: number;
  height: number;
  /** Video only; 0 for an image. */
  durationMs: number;
}

/** What the reader needs from the browser to decode a file's dimensions. */
export interface DecodeEnv {
  image(file: Blob): Promise<{ width: number; height: number }>;
  video(file: Blob): Promise<{ width: number; height: number; durationSeconds: number }>;
}

export const browserDecode: DecodeEnv = {
  async image(file) {
    const bitmap = await createImageBitmap(file);
    const size = { width: bitmap.width, height: bitmap.height };
    bitmap.close();
    return size;
  },
  video(file) {
    return new Promise((resolve, reject) => {
      const url = URL.createObjectURL(file);
      const el = document.createElement("video");
      const done = () => URL.revokeObjectURL(url);
      el.preload = "metadata";
      el.muted = true;
      el.onloadedmetadata = () => {
        resolve({ width: el.videoWidth, height: el.videoHeight, durationSeconds: el.duration });
        done();
      };
      el.onerror = () => {
        reject(new Error("the browser could not read this video"));
        done();
      };
      el.src = url;
    });
  },
};

/** Width and height of an image or video, and the length of a video in milliseconds. */
export async function readMediaInfo(file: Blob, kind: PostMediaKind, env: DecodeEnv = browserDecode): Promise<MediaInfo> {
  if (kind === "POST_MEDIA_KIND_IMAGE") {
    const { width, height } = await env.image(file);
    return { width, height, durationMs: 0 };
  }
  const v = await env.video(file);
  return { width: v.width, height: v.height, durationMs: Number.isFinite(v.durationSeconds) ? Math.round(v.durationSeconds * 1000) : 0 };
}

/** Lower-case hex SHA-256 of a blob, through WebCrypto. */
export async function sha256Hex(file: Blob): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", await file.arrayBuffer());
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

export interface UploadDeps {
  fetch?: typeof fetch;
  decode?: DecodeEnv;
}

export interface UploadRequest {
  repo: RepoRef;
  changeId: string;
  file: Blob & { type: string };
  /** Required for an image, 1 to 1000 characters; optional for a video. */
  altText?: string;
}

/** Refuses what the server would refuse, before any bytes move. */
export function checkUpload(file: Blob & { type: string }, altText: string | undefined): PostMediaKind {
  const kind = mediaKind(file.type);
  if (!kind) throw new BeeError("invalid_argument", { reason: "bad_media", message: (file.type || "this file") + " is not an image (png, jpeg, gif, webp) or a video (mp4, webm)" });
  if (file.size > (kind === "POST_MEDIA_KIND_IMAGE" ? MAX_IMAGE_BYTES : MAX_VIDEO_BYTES)) {
    throw new BeeError("resource_exhausted", { reason: "TOO_LARGE", message: "an image is at most 8 MiB and a video 64 MiB" });
  }
  const alt = (altText ?? "").trim();
  if (kind === "POST_MEDIA_KIND_IMAGE" && (alt.length < 1 || Array.from(alt).length > MAX_ALT_CHARS)) {
    throw new BeeError("invalid_argument", { reason: "alt_text_required", message: "describe the image in 1 to 1000 characters" });
  }
  return kind;
}

/**
 * Begin → PUT → Finalize. Resolves to the finalized media, ready to list in `SetChangePost`.
 * A failed PUT is a `BeeError` with the reason `UPLOAD_FAILED`; Begin and Finalize errors
 * pass through as the server gave them.
 */
export async function uploadPostMedia(client: Pick<BeeClient, "post">, req: UploadRequest, deps: UploadDeps = {}): Promise<PostMedia> {
  const doFetch = deps.fetch ?? fetch;
  const kind = checkUpload(req.file, req.altText);
  const [info, sha256] = await Promise.all([readMediaInfo(req.file, kind, deps.decode), sha256Hex(req.file)]);
  const begin = await client.post.beginPostMediaUpload({ repo: req.repo, changeId: req.changeId, kind, contentType: req.file.type, sizeBytes: String(req.file.size) });
  let res: Response;
  try {
    res = await doFetch(begin.uploadUrl, { method: "PUT", headers: begin.uploadHeaders, body: req.file });
  } catch (err) {
    throw new BeeError("unavailable", { reason: "UPLOAD_FAILED", message: "the upload did not reach the server: " + (err instanceof Error ? err.message : String(err)), retryable: true });
  }
  if (!res.ok) throw new BeeError(res.status >= 500 ? "unavailable" : "invalid_argument", { reason: "UPLOAD_FAILED", message: "the server refused the upload (" + res.status + ")", retryable: res.status >= 500 });
  return client.post.finalizePostMedia({
    repo: req.repo,
    mediaId: begin.mediaId,
    sha256,
    altText: (req.altText ?? "").trim(),
    width: info.width,
    height: info.height,
    durationMs: info.durationMs,
  });
}
