"use client";

import { setFollow } from "./social-actions";
import { SocialToggle } from "./social-toggle";

/** Follow or unfollow a user or agent by login (UserService.FollowUser / UnfollowUser). */
export function FollowButton({ following, count, target, signedIn = true, unavailable, copyAs }: { following: boolean; count?: number; target: string; signedIn?: boolean; unavailable?: string; copyAs?: boolean }) {
  return (
    <SocialToggle
      on={following}
      count={count}
      icon="user"
      labels={{ on: "Following", off: "Follow" }}
      subject={target}
      signedIn={signedIn}
      unavailable={unavailable}
      // Trending already has its page-level Copy as action.
      copyAs={copyAs}
      act={(on) => setFollow(target, on)}
      op={(on) => ({ method: on ? "FollowUser" : "UnfollowUser", request: { login: target }, label: (on ? "Follow " : "Unfollow ") + target })}
    />
  );
}
