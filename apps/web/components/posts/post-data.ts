import { cache } from "react";
import { serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { Post, Principal } from "@/lib/api/types";
import { loginOf } from "./post-text";

export async function optionalRead<T>(read: Promise<T>): Promise<T | undefined> {
  try {
    return await read;
  } catch (err) {
    if (err instanceof BeeError && ["unimplemented", "not_found", "permission_denied", "unauthenticated"].includes(err.code)) return undefined;
    throw err;
  }
}

const membersOf = cache(async (org: string) => optionalRead((await serverClient()).org.listMembers({ org })));
const profileOf = cache(async (owner: string) => optionalRead((await serverClient()).user.getProfile({ owner })));

export async function postOwner(post: Post, me?: Principal | null): Promise<string | undefined> {
  const author = post.author;
  if (author?.kind !== "PRINCIPAL_KIND_AGENT" || !author.ownerId) return;
  if (me?.id === author.ownerId) return me.display;
  if (post.approvedBy?.id === author.ownerId) return post.approvedBy.display;
  const login = loginOf(author);
  const org = post.repo?.fullName?.split("/")[0];
  const [members, profile] = await Promise.all([org && me ? membersOf(org) : undefined, login ? profileOf(login) : undefined]);
  return members?.members.find((member) => member.principal?.id === author.ownerId)?.principal?.display || profile?.ownerLogin;
}
