"use client";

/** The repo header's client parts: the title block, the plain tabs with More, and Clone. */
import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useId, useRef, useState } from "react";
import { isActive, navHref, repoMore, repoTabs } from "@/lib/nav";
import { ownerHref, repoHref } from "@/lib/routes";
import { Icon } from "./icons";
import { Menu, MenuLink, PageMenu } from "./menu";
import type { RepoHeaderProps } from "./repo-header";
import { cloneTabs, type CloneTab } from "./repo/clone-panel";
import { StarButton } from "./star-button";
import { buttonClass, cx } from "./ui";

/** The repo's home and its Code tab get the full Document header; deeper pages a compact one. */
function isFull(path: string, fullName: string): boolean {
  const home = repoHref(fullName);
  return path === home || path === home + "/code";
}

export function RepoHeaderView({ fullName, description, visibility, canWrite, signedIn, star, counts, band, actions }: RepoHeaderProps) {
  const path = usePathname();
  const [owner, name] = fullName.split("/") as [string, string];
  const full = isFull(path, fullName);
  const Title = full ? "h1" : "p";
  return (
    <header className={full ? "mb-10" : "mb-8"}>
      <p className="flex items-center gap-1.5 text-base text-ink-3">
        <Link href={ownerHref(owner)} className="inline-flex min-h-7 items-center text-ink-2 underline decoration-line-strong underline-offset-4 hover:text-ink hover:decoration-ink">
          {owner}
        </Link>
        <span aria-hidden="true">·</span>
        {visibility === "VISIBILITY_PUBLIC" ? (
          <span>public repository</span>
        ) : (
          <span className="inline-flex items-center gap-1">
            <Icon name="lock" size={15} />
            {visibility === "VISIBILITY_INTERNAL" ? "internal repository" : "private repository"}
          </span>
        )}
      </p>
      <div className="mt-1 flex flex-wrap items-start gap-x-6 gap-y-3">
        {/* The repo's name is the page's h1 on its home and Code; deeper pages own their h1. Its
            12rem basis wraps the buttons below it on a phone instead of breaking the name. */}
        <Title className={cx("min-w-0 flex-[1_1_12rem] font-semibold tracking-tight text-ink [overflow-wrap:anywhere]", full ? "text-3xl md:text-[48px] md:leading-[52px]" : "text-2xl")}>
          <Link href={repoHref(fullName)} className="hover:underline hover:decoration-2 hover:underline-offset-4">
            {name}
          </Link>
        </Title>
        <div className={cx("flex shrink-0 items-center gap-2", full ? "md:pt-2" : "")}>
          {actions}
          <StarButton target={fullName} starred={star.starred} count={star.count} unavailable={star.unavailable} signedIn={signedIn} copyAs={false} />
          <PageMenu op={{ service: "RepoService", method: "GetRepoHome", request: { fullName }, label: "Read " + fullName }} label={"More actions for " + fullName} register={false} />
        </div>
      </div>
      {full && description ? <p className="mt-3 max-w-3xl text-lg text-ink-2">{description}</p> : null}
      {full ? band : null}
      <RepoTabs fullName={fullName} canWrite={canWrite} counts={counts} />
    </header>
  );
}

const TAB = "inline-flex h-11 shrink-0 items-center gap-1.5 border-b-2 text-base whitespace-nowrap transition-colors duration-[160ms]";

function RepoTabs({ fullName, canWrite, counts }: { fullName: string; canWrite: boolean; counts: RepoHeaderProps["counts"] }) {
  const path = usePathname();
  const atHome = path === repoHref(fullName);
  const more = repoMore().filter((t) => canWrite || !t.write);
  const inMore = more.find((t) => isActive(t, path, fullName));
  const count = (id: string) => (id === "changes" ? counts.changes : id === "work" ? counts.issues : 0);
  return (
    // More sits outside the scrolling row, so its menu is not clipped by the row's overflow.
    <nav aria-label="Repository sections" className="mt-8 flex items-center gap-6 border-b border-line">
      <ul className="-mb-px flex min-w-0 items-center gap-6 overflow-x-auto scrollbar-thin">
        {repoTabs().map((t) => {
          const active = isActive(t, path, fullName) || (t.id === "code" && atHome);
          const n = count(t.id);
          return (
            <li key={t.id} className="shrink-0">
              <Link href={navHref(t, fullName)} aria-current={active ? "page" : undefined} className={cx(TAB, active ? "border-ink font-medium text-ink" : "border-transparent text-ink-2 hover:border-line-strong hover:text-ink")}>
                {t.label}
                {n ? <span className="num text-ink-3">{n}</span> : null}
              </Link>
            </li>
          );
        })}
      </ul>
      <div className="-mb-px shrink-0">
        <Menu
          label="More repository sections"
          place="below-end"
          triggerClass={cx(TAB, inMore ? "border-ink font-medium text-ink" : "border-transparent text-ink-2 hover:border-line-strong hover:text-ink")}
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
  const [open, setOpen] = useState(false);
  const [copied, copy] = useCopied();
  const id = useId();
  const root = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const lines = cloneTabs({ fullName, https, ssh }).filter((t): t is CloneTab & { command: string } => !!t.command);
  useEffect(() => {
    if (!open) return;
    const away = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setOpen(false);
        button.current?.focus();
      }
    };
    document.addEventListener("pointerdown", away);
    document.addEventListener("keydown", esc);
    return () => {
      document.removeEventListener("pointerdown", away);
      document.removeEventListener("keydown", esc);
    };
  }, [open]);
  return (
    <div ref={root} className="relative">
      <button ref={button} type="button" aria-expanded={open} aria-controls={id} onClick={() => setOpen((v) => !v)} className={buttonClass("secondary", "sm", "bg-bg")}>
        Clone
        <Icon name="chevronDown" size={14} weight="bold" />
      </button>
      {open ? (
        <div id={id} role="group" aria-label={"Clone " + fullName} className="absolute top-[calc(100%+6px)] right-0 z-40 flex w-[min(460px,calc(100vw-40px))] flex-col gap-4 rounded-xl border border-line bg-popover p-4 shadow-overlay animate-enter">
          {lines.map((t) => (
            <div key={t.id}>
              <p className="mb-1.5 text-sm font-medium text-ink">{t.label}</p>
              <div className="flex items-center gap-2 rounded-md bg-sunken py-1 pr-1 pl-3">
                <code className="min-w-0 flex-1 truncate text-sm text-ink" tabIndex={0} aria-label={t.label + " command"}>
                  {t.command}
                </code>
                <button type="button" className={buttonClass("ghost", "sm", "size-8 px-0")} aria-label={"Copy the " + t.label + " command"} onClick={() => copy(t.id, t.command)}>
                  <Icon name={copied === t.id ? "check" : "copy"} size={16} />
                </button>
              </div>
              {t.hint ? <p className="mt-1.5 text-sm text-ink-3">{t.hint}</p> : null}
            </div>
          ))}
        </div>
      ) : null}
    </div>
  );
}
