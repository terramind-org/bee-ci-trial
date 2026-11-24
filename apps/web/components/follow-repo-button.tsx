"use client";

import { setRepoFollow } from "./post-actions";
import { SocialToggle } from "./social-toggle";

/**
 * Follow or unfollow a repo (UserService.FollowRepo / UnfollowRepo), with its follower count.
 * Following fills the reader's Feed with the repo's posts; it is separate from Star (a bookmark)
 * and Watch (notifications). Mounted in the repo header's `actions` slot, whose "…" menu is the
 * page's Copy as, so the toggle has none of its own.
 */
export function FollowRepoButton({ following, count, target, signedIn = true, unavailable }: { following: boolean; count?: number; target: string; signedIn?: boolean; unavailable?: string }) {
  return (
    <SocialToggle
      on={following}
      count={count}
      icon="feed"
      labels={{ on: "Following", off: "Follow" }}
      subject={target}
      signedIn={signedIn}
      unavailable={unavailable}
      copyAs={false}
      act={(on) => setRepoFollow(target, on)}
      op={(on) => ({ method: on ? "FollowRepo" : "UnfollowRepo", request: { repo: { fullName: target } }, label: (on ? "Follow " : "Unfollow ") + target })}
    />
  );
}
