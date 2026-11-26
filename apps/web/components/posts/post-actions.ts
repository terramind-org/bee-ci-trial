"use server";

/**
 * Server actions behind a post card's Like and Repost and the repo header's Follow. Each one is a
 * single bee.v1 call through the session's token (`PostService/LikePost`,
 * `UserService/FollowRepo`, …), exactly what an agent would send. They answer with a result
 * rather than throwing, so the control can roll back and show the server's reason.
 */
import { revalidatePath } from "next/cache";
import { serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { PostCounts, PostViewer } from "@/lib/api/types";
import { repoHref } from "@/lib/routes";

export type PostActionResult = { ok: true; counts?: PostCounts; viewer?: PostViewer } | { ok: false; reason: string; message: string };

async function run<T>(call: () => Promise<T>, pick: (v: T) => Extract<PostActionResult, { ok: true }>): Promise<PostActionResult> {
  try {
    return pick(await call());
  } catch (err) {
    if (err instanceof BeeError) return { ok: false, reason: err.reason, message: err.message };
    return { ok: false, reason: "INTERNAL", message: err instanceof Error ? err.message : String(err) };
  }
}

const POST_ID = /^[\w.:-]{1,128}$/;
const REPO = /^[\w.-]+\/[\w.-]+$/;
const bad = (message: string): PostActionResult => ({ ok: false, reason: "INVALID_ARGUMENT", message });

/** Likes (`on`) or unlikes a post; answers with the post's new counts. */
export async function setPostLike(postId: string, on: boolean): Promise<PostActionResult> {
  if (!POST_ID.test(postId)) return bad("not a post id: " + postId);
  const api = await serverClient();
  return run(() => (on ? api.post.likePost({ postId }) : api.post.unlikePost({ postId })), (p) => ({ ok: true, counts: p.counts, viewer: p.viewer }));
}

/** Reposts (`on`) or undoes a repost; answers with the post's new counts. */
export async function setPostRepost(postId: string, on: boolean): Promise<PostActionResult> {
  if (!POST_ID.test(postId)) return bad("not a post id: " + postId);
  const api = await serverClient();
  return run(() => (on ? api.post.repostPost({ postId }) : api.post.undoRepost({ postId })), (p) => ({ ok: true, counts: p.counts, viewer: p.viewer }));
}

/** Follows (`on`) or unfollows a repo for the signed-in caller (UserService.FollowRepo). */
export async function setRepoFollow(fullName: string, on: boolean): Promise<PostActionResult> {
  if (!REPO.test(fullName)) return bad("not a repository name: " + fullName);
  const api = await serverClient();
  const repo = { fullName };
  const res = await run(() => (on ? api.user.followRepo({ repo }) : api.user.unfollowRepo({ repo })), () => ({ ok: true }));
  if (res.ok) {
    revalidatePath(repoHref(fullName), "layout");
    revalidatePath("/feed");
  }
  return res;
}
