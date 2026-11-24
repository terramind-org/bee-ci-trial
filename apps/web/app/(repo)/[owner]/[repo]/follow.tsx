/**
 * The repo header's Follow button with its state: whether the caller follows the repo
 * (UserService.IsFollowingRepo, signed in only) and how many do (ListRepoFollowers, paged,
 * because the response carries no total). It streams inside the header's `actions` slot, so the
 * header never waits on it.
 */
import { FollowRepoButton } from "@/components/follow-repo-button";
import { serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";

const PAGE = 200;
/** Followers counted before the count stops: ten pages. */
export const FOLLOWER_COUNT_CAP = PAGE * 10;

export async function followerCount(list: (pageToken: string) => Promise<{ n: number; next: string }>): Promise<number> {
  let n = 0;
  let token = "";
  do {
    const page = await list(token);
    n += page.n;
    token = page.next;
  } while (token && n < FOLLOWER_COUNT_CAP);
  return Math.min(n, FOLLOWER_COUNT_CAP);
}

export async function RepoFollow({ fullName, signedIn }: { fullName: string; signedIn: boolean }) {
  const api = await serverClient();
  const repo = { fullName };
  const [count, following] = await Promise.allSettled([
    followerCount(async (pageToken) => {
      const res = await api.user.listRepoFollowers({ repo, page: { pageSize: PAGE, pageToken } });
      return { n: res.followers.length, next: res.page?.nextPageToken ?? "" };
    }),
    signedIn ? api.user.isFollowingRepo({ repo }).then((r) => r.following) : Promise.resolve(false),
  ]);
  const unimplemented = [count, following].some((r) => r.status === "rejected" && r.reason instanceof BeeError && r.reason.code === "unimplemented");
  return (
    <FollowRepoButton
      target={fullName}
      signedIn={signedIn}
      following={following.status === "fulfilled" ? following.value : false}
      count={count.status === "fulfilled" ? count.value : undefined}
      unavailable={unimplemented ? "Following is not available on this server (UNIMPLEMENTED)" : undefined}
    />
  );
}
