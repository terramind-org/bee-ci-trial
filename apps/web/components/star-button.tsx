"use client";

import { setRepoStar } from "./social-actions";
import { SocialToggle } from "./social-toggle";

/** Star or unstar a repo (UserService.StarRepo / UnstarRepo), with its star count. */
export function StarButton({ starred, count, target, signedIn = true, unavailable, copyAs = true }: { starred: boolean; count?: number; target: string; signedIn?: boolean; unavailable?: string; copyAs?: boolean }) {
  return (
    <SocialToggle
      on={starred}
      count={count}
      icon="star"
      labels={{ on: "Starred", off: "Star" }}
      subject={target}
      signedIn={signedIn}
      unavailable={unavailable}
      copyAs={copyAs}
      act={(on) => setRepoStar(target, on)}
      op={(on) => ({ method: on ? "StarRepo" : "UnstarRepo", request: { repo: { fullName: target } }, label: (on ? "Star " : "Unstar ") + target })}
    />
  );
}
