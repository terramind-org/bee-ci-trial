/**
 * The rank row (docs/design/tokens.md, "Rank row"): the rank, an avatar, the subject, then the
 * score. Rows are `j`/`k` targets through `data-nav-item` on their link. The top three differ
 * only by the weight of their number and name, never by colour. A row may carry a hairline bar
 * under it showing its share of the list; the bar is decoration, the share is also printed.
 */
import Link from "next/link";
import type { ReactNode } from "react";
import { Avatar } from "@/components/avatar";
import { PrincipalMark } from "@/components/domain";
import { cx } from "@/components/ui";
import type { Principal, SearchRepoHit, SearchUserHit } from "@/lib/api/types";
import { ownerHref, repoHref } from "@/lib/routes";

export const isTop = (rank: number) => rank >= 1 && rank <= 3;

export function Rank({ rank }: { rank: number }) {
  return <span className={cx("num w-8 shrink-0 text-right text-lg leading-7", isTop(rank) ? "font-semibold text-ink" : "text-ink-3")}>{rank}</span>;
}

/** A search hit's principal, or the shape its login implies when the server left it out. */
export function principalOf(hit: SearchUserHit): Principal {
  return hit.principal ?? { id: hit.login, kind: "PRINCIPAL_KIND_HUMAN", display: hit.name || hit.login, ownerId: "", runtime: "", externalIds: [] };
}

/** The mark of whoever owns a repository: the owner's login seeds it, so every repo of one owner shares it. */
export function OwnerMark({ fullName, size = 36 }: { fullName: string; size?: number }) {
  const owner = fullName.split("/")[0] ?? fullName;
  return <Avatar principal={{ id: owner, name: owner, kind: "human" }} size={size} />;
}

/** Mark, login as a link and, for an agent, its runtime. */
export function PrincipalCell({ hit, top = false, nav = true, size = 36 }: { hit: SearchUserHit; top?: boolean; nav?: boolean; size?: number }) {
  const p = principalOf(hit);
  return (
    <span className="inline-flex min-w-0 items-center gap-3">
      <PrincipalMark principal={p} size={size} />
      <Link href={ownerHref(hit.login)} className={cx("min-w-0 truncate text-md text-ink hover:underline", top ? "font-semibold" : "font-medium")} data-nav-item={nav ? "" : undefined}>
        {hit.login}
      </Link>
      {p.kind === "PRINCIPAL_KIND_AGENT" && p.runtime ? <span className="shrink-0 text-sm text-ink-3">{p.runtime}</span> : null}
      {hit.name && hit.name !== hit.login ? <span className="hidden min-w-0 truncate text-base text-ink-3 sm:inline">{hit.name}</span> : null}
    </span>
  );
}

/** A repository's name as a link: the owner in a lighter tone, the name in the strong one. */
export function RepoName({ fullName, top = false, nav = true, className }: { fullName: string; top?: boolean; nav?: boolean; className?: string }) {
  const slash = fullName.indexOf("/");
  const owner = slash < 0 ? "" : fullName.slice(0, slash + 1);
  const name = slash < 0 ? fullName : fullName.slice(slash + 1);
  return (
    <Link href={repoHref(fullName)} className={cx("min-w-0 truncate text-md hover:underline", className)} data-nav-item={nav ? "" : undefined}>
      <span className="text-ink-3">{owner}</span>
      <span className={cx("text-ink", top ? "font-semibold" : "font-medium")}>{name}</span>
    </Link>
  );
}

export function RepoCell({ hit, top = false, nav = true, size = 36 }: { hit: SearchRepoHit; top?: boolean; nav?: boolean; size?: number }) {
  return (
    <span className="inline-flex min-w-0 items-center gap-3">
      <OwnerMark fullName={hit.fullName} size={size} />
      <RepoName fullName={hit.fullName} top={top} nav={nav} />
    </span>
  );
}

/** The share of a list as a 2px line in `ink-3` (cobalt is for actions). */
export function ShareBar({ share, className }: { share: number; className?: string }) {
  const pct = Math.max(0, Math.min(100, Math.round(share * 100)));
  return (
    <span aria-hidden="true" className={cx("block h-0.5 w-full bg-line", className)}>
      <span className="block h-0.5 bg-ink-3" style={{ width: Math.max(2, pct) + "%" }} />
    </span>
  );
}

/** One row of a rank list: rank, an optional avatar, the subject (flexible), and optional trailing content. */
export function RankRow({
  rank,
  avatar,
  children,
  trailing,
  share,
  className,
}: {
  rank: number;
  avatar?: ReactNode;
  children: ReactNode;
  trailing?: ReactNode;
  /** 0 to 1: draws the hairline share bar under the row. */
  share?: number;
  className?: string;
}) {
  return (
    <li className={cx("py-3.5", className)} data-rank={rank}>
      <div className="flex flex-wrap items-start gap-x-4 gap-y-3 sm:flex-nowrap">
        <Rank rank={rank} />
        {avatar ? <div className="shrink-0">{avatar}</div> : null}
        <div className="min-w-[10rem] flex-1 basis-0 self-center">{children}</div>
        {trailing ? <div className="order-last flex basis-full items-center gap-3 pl-12 sm:order-none sm:shrink-0 sm:basis-auto sm:self-center sm:pl-0">{trailing}</div> : null}
      </div>
      {share !== undefined ? <ShareBar share={share} className="mt-3" /> : null}
    </li>
  );
}
