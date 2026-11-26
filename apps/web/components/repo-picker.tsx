"use client";

import { usePathname, useRouter } from "next/navigation";
import { REPO_COOKIE } from "@/lib/api/config";
import { repoFromPath, repoHref, repoSubPath } from "@/lib/routes";

/** Remembers the last repo, for the legacy root-level redirects in proxy.ts. */
export function setRepoCookie(fullName: string) {
  document.cookie = REPO_COOKIE + "=" + encodeURIComponent(fullName) + "; path=/; max-age=31536000; samesite=lax";
}

/**
 * Where switching to `fullName` goes from `path`: the same section of the other repo when on a
 * repo page (ids do not carry across repos, so only the section), else the repo's code.
 */
export function switchTarget(path: string, fullName: string): string {
  const section = repoFromPath(path) ? repoSubPath(path).split("/")[0] : "";
  return repoHref(fullName, section || "code");
}

export function RepoPicker({ repo, repos, id = "repo-picker" }: { repo: string; repos: string[]; id?: string }) {
  const router = useRouter();
  const path = usePathname();
  return (
    <div className="flex">
      <label htmlFor={id} className="sr-only">
        Repository
      </label>
      <select
        id={id}
        value={repo}
        onChange={(e) => {
          setRepoCookie(e.target.value);
          router.push(switchTarget(path, e.target.value));
        }}
        className="h-9 w-full cursor-pointer truncate rounded-md border border-transparent bg-transparent px-1.5 text-base font-medium text-ink transition-colors duration-[120ms] hover:border-line-strong hover:bg-hover"
      >
        {repos.map((r) => (
          <option key={r} value={r}>
            {r}
          </option>
        ))}
      </select>
    </div>
  );
}
