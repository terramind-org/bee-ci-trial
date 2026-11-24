/**
 * The "Inspired by" banner for a change request or issue page: loads the source post as the reader
 * and renders nothing when they cannot read it (or it is no longer published).
 */
import { cache } from "react";
import { serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { Post, PostRef } from "@/lib/api/types";
import { InspiredBy, type InspiredPost } from "./inspired-by";
import { mediaHref } from "./post-text";

/** What the banner needs of a post, or null when the reader cannot read it. */
export const loadInspiredPost = cache(async (ref: PostRef): Promise<{ repo: string; post: InspiredPost } | null> => {
  const api = await serverClient();
  let post: Post;
  try {
    post = await api.post.getPost({ postId: ref.postId });
  } catch (err) {
    if (err instanceof BeeError && (err.code === "not_found" || err.code === "permission_denied")) return null;
    throw err;
  }
  if (post.state !== "POST_STATE_PUBLISHED") return null;
  // The server may name a repo by id only; the permalink needs its name.
  let name = ref.repo?.fullName || post.repo?.fullName || "";
  const id = ref.repo?.id || post.repo?.id;
  if (!name && id) {
    try {
      name = (await api.repo.getRepo({ repo: { id } })).fullName;
    } catch (err) {
      if (!(err instanceof BeeError)) throw err;
    }
  }
  if (!name) return null;
  const image = post.media.find((m) => m.kind === "POST_MEDIA_KIND_IMAGE");
  return { repo: name, post: { text: post.text, thumb: image ? { src: mediaHref({ repo: { fullName: name }, id: post.id }, image), alt: image.altText } : undefined } };
});

/** The banner for a change request or issue: renders nothing without a readable source post. */
export async function InspiredByBanner({ inspiredBy }: { inspiredBy: PostRef | undefined }) {
  if (!inspiredBy?.postId) return null;
  const loaded = await loadInspiredPost(inspiredBy);
  return loaded ? <InspiredBy inspiredBy={inspiredBy} repo={loaded.repo} post={loaded.post} /> : null;
}
