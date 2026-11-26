"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import type { ComponentProps } from "react";
import { repoFromPath, repoHref } from "@/lib/routes";

/**
 * A link to a page of the repo the visitor is in, for shared components that do not know the
 * repo (agent names, for one). Outside a repo it uses the root-level path, which proxy.ts sends on
 * to the last repo visited.
 */
export function RepoLink({ sub, ...rest }: { sub: string } & Omit<ComponentProps<typeof Link>, "href">) {
  const repo = repoFromPath(usePathname());
  return <Link href={repo ? repoHref(repo, sub) : "/" + sub.replace(/^\/+/, "")} {...rest} />;
}
