"use client";

/** The repo header's client parts: the title line, the tabs with More, and Clone. */
import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useState } from "react";
import { isActive, navHref, repoMore, repoTabs } from "@/lib/nav";
import { ownerHref, repoHref } from "@/lib/routes";
import { Icon } from "./icons";
import { State } from "./parts";
import { ForkButton, SyncForkButton } from "./repo-header-forks";
import { Popover } from "./kit/popover";
import { tabClass } from "./kit/tabs-style";
import { Menu, MenuLink, PageMenu } from "./menu";
import type { RepoHeaderProps } from "./repo-header";
import { cloneTabs, type CloneTab } from "./repo/clone-panel";
import { StarButton } from "./star-button";
import { Button, buttonClass } from "./ui";

/** The info band (branch, files, last merge, Clone) shows on the repo's home and its Code tab. */
function isFull(path: string, fullName: string): boolean {
  const home = repoHref(fullName);
  return path === home || path === home + "/code";
}

const VISIBILITY: Record<string, string> = { VISIBILITY_PUBLIC: "Public", VISIBILITY_INTERNAL: "Internal" };

export function RepoHeaderView({ fullName, description, visibility, canWrite, signedIn, star, forks, counts, band, actions, upstream, agent, canSync }: RepoHeaderProps) {
  const path = usePathname();
  const [owner, name] = fullName.split("/") as [string, string];
  const full = isFull(path, fullName);
  // The repo's name is the page's h1 on its home and Code; deeper pages own their h1.
  const Name = full ? "h1" : "span";
  return (
    <header className="mb-10">
      <div className="mb-3.5 flex flex-wrap items-center gap-x-2.5 gap-y-2 text-base text-ink-2">
        <span className="inline-flex min-w-0 items-center gap-1.5">
          <Link href={ownerHref(owner)} className="link min-h-6 break-all py-0.5 text-ink-2">
            {owner}
          </Link>
          <span aria-hidden="true">/</span>
          <Name className="min-w-0 text-base font-medium tracking-normal text-ink [overflow-wrap:anywhere]">
            <Link href={repoHref(fullName)} className="inline-flex min-h-6 items-center hover:underline hover:underline-offset-4">
              {name}
            </Link>
          </Name>
        </span>
        <State shape={visibility === "VISIBILITY_PUBLIC" ? "open" : "closed"} className="text-sm text-ink-2">{VISIBILITY[visibility] ?? "Private"}</State>
        {description ? <span className="min-w-0 basis-full [overflow-wrap:anywhere] lg:basis-auto lg:flex-1 lg:truncate">{description}</span> : null}
        {!agent ? (
          <div className="ml-auto flex max-w-full flex-wrap items-center gap-2">
            {actions}
            <StarButton target={fullName} starred={star.starred} count={star.count} unavailable={star.unavailable} signedIn={signedIn} copyAs={false} />
            <ForkButton key={fullName} fullName={fullName} count={forks} signedIn={signedIn} />
            <PageMenu op={{ service: "RepoService", method: "GetRepoHome", request: { fullName }, label: "Read " + fullName }} label={"More actions for " + fullName} register={false} />
          </div>
        ) : null}
      </div>
      {upstream && !agent ? (
        <div className="mb-4 flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-ink-2">
          <span>Forked from <Link href={repoHref(upstream.fullName)} className="link inline-flex min-h-6 items-center">{upstream.fullName}</Link></span>
          {canSync && upstream.behind ? <SyncForkButton fullName={fullName} upstream={upstream.fullName} /> : null}
        </div>
      ) : null}
      {full && !agent ? band : null}
      <RepoTabs fullName={fullName} canWrite={canWrite} counts={counts} />
      {/* Insights owns the Forks subpage without expanding the repository's tab set. */}
      {path.startsWith(repoHref(fullName, "insights")) ? (
        <nav aria-label="Repository insights" className="mt-3 flex gap-4 text-sm">
          <Link href={repoHref(fullName, "insights")} className="link inline-flex min-h-6 items-center" aria-current={path === repoHref(fullName, "insights") ? "page" : undefined}>Overview</Link>
          <Link href={repoHref(fullName, "insights/forks")} className="link inline-flex min-h-6 items-center" aria-current={path === repoHref(fullName, "insights/forks") ? "page" : undefined}>Forks</Link>
        </nav>
      ) : null}
    </header>
  );
}

function RepoTabs({ fullName, canWrite, counts }: { fullName: string; canWrite: boolean; counts: RepoHeaderProps["counts"] }) {
  const path = usePathname();
  const atHome = path === repoHref(fullName);
  const more = repoMore();
  const inMore = more.find((t) => isActive(t, path, fullName));
  const count = (id: string) => (id === "changes" ? counts.changes : id === "work" ? counts.issues : 0);
  return (
    // More sits outside the scrolling row, so its menu is not clipped by the row's overflow.
    <nav aria-label="Repository sections" className="flex items-center gap-6 border-b border-line">
      <ul className="-mb-px flex min-w-0 items-center gap-6 overflow-x-auto scrollbar-thin">
        {repoTabs(canWrite).map((t) => {
          // Code opens the repository's home (README, files, commits); the file browser is one step in.
          const active = isActive(t, path, fullName) || (t.id === "code" && atHome);
          const n = count(t.id);
          return (
            <li key={t.id} className="shrink-0">
              <Link href={t.id === "code" ? repoHref(fullName) : navHref(t, fullName)} aria-current={active ? "page" : undefined} className={tabClass(active)}>
                {t.label}
                {n ? <span className="num text-xs text-ink-3">{n}</span> : null}
              </Link>
            </li>
          );
        })}
      </ul>
      <div className="-mb-px shrink-0">
        <Menu
          label="More repository sections"
          place="below-end"
          triggerClass={tabClass(!!inMore)}
          trigger={
            <>
              {inMore ? inMore.label : "More"}
              <Icon name="chevronDown" size={14} weight="bold" />
            </>
          }
        >
          {more.map((t) => (
            <MenuLink key={t.id} href={navHref(t, fullName)} icon={t.icon}>
              {t.label}
            </MenuLink>
          ))}
        </Menu>
      </div>
    </nav>
  );
}

function useCopied(): [string | null, (key: string, text: string) => void] {
  const [copied, setCopied] = useState<string | null>(null);
  useEffect(() => {
    if (!copied) return;
    const t = setTimeout(() => setCopied(null), 1600);
    return () => clearTimeout(t);
  }, [copied]);
  return [copied, (key, text) => void navigator.clipboard?.writeText(text).then(() => setCopied(key))];
}

/** One Clone button; HTTPS, SSH (when the server has it) and the Bee CLI inside. */
export function CloneMenu({ fullName, https, ssh }: { fullName: string; https: string; ssh?: string }) {
  const [copied, copy] = useCopied();
  const lines = cloneTabs({ fullName, https, ssh }).filter((t): t is CloneTab & { command: string } => !!t.command);
  return (
    <Popover
      title={"Clone " + fullName}
      triggerClass={buttonClass("secondary", "sm")}
      trigger={
        <>
          Clone
          <Icon name="chevronDown" size={14} weight="bold" />
        </>
      }
      className="flex w-[min(460px,calc(100vw-40px))] flex-col gap-4"
    >
      {lines.map((t) => (
        <div key={t.id}>
          <p className="mb-1.5 text-xs font-medium text-ink-2">{t.label}</p>
          <div className="flex items-center gap-2 rounded-xl bg-sunken py-1 pr-1 pl-3">
            <code className="min-w-0 flex-1 truncate text-sm text-ink" tabIndex={0} aria-label={t.label + " command"}>
              {t.command}
            </code>
            <Button type="button" variant="ghost" size="sm" className="size-8 px-0" aria-label={"Copy the " + t.label + " command"} onClick={() => copy(t.id, t.command)}>
              <Icon name={copied === t.id ? "check" : "copy"} size={16} />
            </Button>
          </div>
          {t.hint ? <p className="mt-1.5 text-sm text-ink-3">{t.hint}</p> : null}
        </div>
      ))}
    </Popover>
  );
}
