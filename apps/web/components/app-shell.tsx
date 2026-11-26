"use client";

/**
 * The app shell: the one navigation system on every page (docs/spec/web-ui.md §2).
 *
 *   signed in   a left rail: the wordmark, then Needs you (with its count), Feed and
 *               Repositories; the organisation's People and teams, Agents and budgets and
 *               Record; the repositories this browser opened last; and at the foot the person
 *               (a link to their profile), the Person · Agent switch and the theme switch.
 *               Below `md` the rail is a drawer behind a slim top bar.
 *   signed out  a slim top bar: the wordmark, Explore (and Pricing on cloud), search, Sign in.
 *
 * It also mounts the keyboard chrome once: the `g` chords, `?`, `/`, j/k, `y` Copy-as and ⌘K.
 * Pages render inside `<main id="main">` and choose their own measure (see docs/design/tokens.md).
 * In the agent's view (`mode === "bee"`) the page opens with the agent's header.
 */
import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import { useEffect, useState, type ReactNode } from "react";
import type { Principal } from "@/lib/api/types";
import { canSwitchMode, type Mode } from "@/lib/mode";
import { NAV, SHORTCUTS, isActive, navHref, orgRailItems, railItems, repoMore, repoTabs } from "@/lib/nav";
import { isRepoFullName, ownerHref, repoHref } from "@/lib/routes";
import { BeeHeader } from "./bee-header";
import { CommandPalette, openPalette } from "./command-palette";
import { GlobalCopyAs } from "./copy-as";
import { Icon } from "./icons";
import { Dialog, DialogBackdrop, DialogClose, DialogContent, DialogTitle, DialogTrigger, Modal } from "./kit/dialog";
import { ModeSwitch } from "./mode-switch";
import { useRecentRepos } from "./recent-repos";
import { setRepoCookie } from "./repo-picker";
import { ThemeSwitch } from "./theme";
import { Kbd, buttonClass, cx } from "./ui";

/** What the keyboard chrome and the palette resolve against. */
export interface ChromeProps {
  /** The repo shortcuts and the palette resolve against: the URL's, else the remembered one. */
  repo: string | null;
  repos: string[];
  org: string;
  canWrite?: boolean;
  signedIn?: boolean;
}

/** The repository a repo page is under, for the drawer's sections and the Recent list. */
export interface ShellRepo {
  fullName: string;
  canWrite: boolean;
}

export interface AppShellProps {
  me: Principal | null;
  edition: "cloud" | "enterprise";
  adapter: "http" | "fixture";
  /** The organisation the rail's second group belongs to; "" when the person is in none. */
  org: string;
  /** Person or agent view for this request (lib/mode.ts). */
  mode: Mode;
  /**
   * The count beside "Needs you". A server component in a Suspense boundary
   * (app/inbox-count.tsx), so the rail never waits for it; it renders nothing at zero.
   */
  inboxBadge?: ReactNode;
  chrome: ChromeProps;
  /** Set on repo pages. */
  repo?: ShellRepo | null;
  children: ReactNode;
}

function isTyping(t: EventTarget | null): boolean {
  const el = t as HTMLElement | null;
  return !!el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName));
}

/* ---------------------------------------------------------------- keyboard chrome */

function ShortcutsDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (v: boolean) => void }) {
  return (
    <Modal open={open} onOpenChange={onOpenChange} title="Keyboard shortcuts" className="top-[8vh] max-h-[84vh]">
      <dl className="grid grid-cols-[auto_1fr] gap-x-5 gap-y-2.5">
        {SHORTCUTS.map((s) => (
          <div key={s.label} className="contents">
            <dt className="flex gap-1">
              {s.keys.map((k) => (
                <Kbd key={k}>{k}</Kbd>
              ))}
            </dt>
            <dd className="text-base text-ink-2">{s.label}</dd>
          </div>
        ))}
      </dl>
    </Modal>
  );
}

/**
 * g-chords (resolved against the current repo), `/`, `?`, and j/k list navigation over
 * [data-nav-item]. Without a repo, repo-scoped chords do nothing (the rail's Agents chord opens
 * the agent leaderboard instead).
 */
function useKeyboard(setHelp: (v: boolean) => void, repo: string | null, org: string, signedIn: boolean) {
  const router = useRouter();
  useEffect(() => {
    let chord = 0;
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey || isTyping(e.target) || document.querySelector("[role=dialog],[role=alertdialog],[role=menu]")) return;
      if (chord && Date.now() - chord < 1200) {
        chord = 0;
        const item = NAV.find((n) => n.key === e.key);
        const reachable = item && (item.target.scope !== "repo" || repo || item.id === "agents") && (item.target.scope !== "org" || org) && (signedIn || !item.signedIn);
        if (item && reachable) {
          e.preventDefault();
          router.push(navHref(item, repo ?? org + "/", org));
        }
        return;
      }
      if (e.key === "g") {
        chord = Date.now();
        return;
      }
      if (e.key === "?") {
        e.preventDefault();
        setHelp(true);
        return;
      }
      if (e.key === "/") {
        const f = document.querySelector<HTMLElement>("[data-filter]");
        if (f) {
          e.preventDefault();
          f.focus();
        }
        return;
      }
      if (e.key === "j" || e.key === "k") {
        const items = [...document.querySelectorAll<HTMLElement>("[data-nav-item]")];
        if (!items.length) return;
        e.preventDefault();
        const i = items.indexOf(document.activeElement as HTMLElement);
        const n = e.key === "j" ? Math.min(items.length - 1, i + 1) : Math.max(0, i === -1 ? 0 : i - 1);
        items[n]?.focus();
        items[n]?.scrollIntoView({ block: "nearest" });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [router, setHelp, repo, org, signedIn]);
}

/** Keyboard, command palette, `y` Copy-as and the shortcuts list. Mounted once by AppShell. */
export function Chrome({ repo, repos, org, canWrite = true, signedIn = true }: ChromeProps) {
  const [help, setHelp] = useState(false);
  const usable = repo && isRepoFullName(repo) ? repo : null;
  useKeyboard(setHelp, usable, org, signedIn);
  return (
    <>
      <CommandPalette repo={usable} repos={repos} org={org} canWrite={canWrite} signedIn={signedIn} />
      <GlobalCopyAs />
      <ShortcutsDialog open={help} onOpenChange={setHelp} />
    </>
  );
}

/* ---------------------------------------------------------------- pieces */

/** The Beehive wordmark: the yellow dot, then the name. */
export function Wordmark({ onClick }: { onClick?: () => void }) {
  return (
    <Link href="/" onClick={onClick} aria-label="Beehive home" className="inline-flex h-10 shrink-0 items-center gap-2 rounded-md px-2.5 text-[19px] font-medium tracking-[-0.02em] text-ink">
      <span className="dot" aria-hidden="true" />
      Beehive
    </Link>
  );
}

function SearchField({ id, action, label, placeholder, className }: { id: string; action: string; label: string; placeholder: string; className?: string }) {
  return (
    <form action={action} role="search" className={cx("relative", className)}>
      <label htmlFor={id} className="sr-only">
        {label}
      </label>
      <Icon name="search" size={17} className="pointer-events-none absolute top-1/2 left-3.5 -translate-y-1/2 text-ink-3" />
      <input id={id} name="q" type="search" placeholder={placeholder} className="h-[34px] w-full rounded-full bg-input pr-3 pl-10 text-xs text-ink shadow-[inset_0_0_0_1px_var(--line)] placeholder:text-ink-3 hover:shadow-[inset_0_0_0_1px_var(--line-strong)]" />
    </form>
  );
}

const RAIL_ITEM = "flex min-h-9 w-full items-center justify-between gap-3 rounded-[10px] px-2.5 py-[7px] transition-colors duration-[160ms]";
const RAIL_GROUP = "eyebrow mx-2.5 mt-5 mb-1.5 truncate";

function RailLink({ label, href, active, badge, small = false, onNavigate }: { label: ReactNode; href: string; active: boolean; badge?: ReactNode; small?: boolean; onNavigate?: () => void }) {
  return (
    <li>
      <Link href={href} onClick={onNavigate} aria-current={active ? "page" : undefined} className={cx(RAIL_ITEM, small ? "text-sm" : "text-md", active ? "bg-selected text-ink" : "text-ink-2 hover:bg-hover hover:text-ink")}>
        <span className="min-w-0 truncate">{label}</span>
        {badge}
      </Link>
    </li>
  );
}

/** `owner / name`, the way the repository header writes it. */
function repoLabel(fullName: string): string {
  return fullName.replace("/", " / ");
}

/** The rail's body, shared by the desktop rail and the mobile drawer. */
function RailBody({ props, onNavigate, drawer = false }: { props: AppShellProps; onNavigate?: () => void; drawer?: boolean }) {
  const path = usePathname();
  const { me, repo, chrome, inboxBadge, org } = props;
  const scope = repo?.fullName ?? chrome.repo ?? org + "/";
  const recent = useRecentRepos(repo?.fullName);
  const orgItems = orgRailItems(!!me, org);
  return (
    <>
      <nav aria-label="Main">
        <ul className="flex flex-col gap-0.5">
          {railItems(!!me).map((item) => {
            // Inside a repository, Repositories is where you are.
            const active = item.id === "repositories" && repo ? true : isActive(item, path, scope, org);
            return <RailLink key={item.id} label={item.label} href={navHref(item, scope, org)} active={active} badge={item.id === "inbox" ? inboxBadge : undefined} onNavigate={onNavigate} />;
          })}
        </ul>
      </nav>
      {orgItems.length ? (
        <nav aria-label={"Organization " + org}>
          <p className={RAIL_GROUP}>{org}</p>
          <ul className="flex flex-col gap-0.5">
            {orgItems.map((item) => (
              <RailLink key={item.id} label={item.label} href={navHref(item, scope, org)} active={isActive(item, path, scope, org)} onNavigate={onNavigate} />
            ))}
          </ul>
        </nav>
      ) : null}
      {drawer && repo ? (
        <nav aria-label="Repository sections">
          <p className={RAIL_GROUP}>{repoLabel(repo.fullName)}</p>
          <ul className="flex flex-col gap-0.5">
            {[...repoTabs(repo.canWrite), ...repoMore()].map((n) => (
              <RailLink key={n.id} label={n.label} href={navHref(n, repo.fullName)} active={isActive(n, path, repo.fullName)} small onNavigate={onNavigate} />
            ))}
          </ul>
        </nav>
      ) : null}
      {recent.length ? (
        <nav aria-label="Recent repositories">
          <p className={RAIL_GROUP}>Recent</p>
          <ul className="flex flex-col gap-0.5">
            {recent.map((r) => (
              <RailLink key={r} label={repoLabel(r)} href={repoHref(r)} active={false} small onNavigate={onNavigate} />
            ))}
          </ul>
        </nav>
      ) : null}
    </>
  );
}

/**
 * The rail's foot: who is signed in (a link to their profile), the Person · Agent switch for
 * people, and the theme. A person carries the yellow mark; an agent is named with who it is for.
 */
function RailFoot({ me, mode, onNavigate }: { me: Principal; mode: Mode; onNavigate?: () => void }) {
  const name = me.display || me.id;
  const agent = me.kind === "PRINCIPAL_KIND_AGENT";
  return (
    <div className="mt-auto grid gap-2.5 pt-5">
      <Link href={ownerHref(name)} onClick={onNavigate} className="mx-1 flex min-h-8 items-center gap-2 rounded-md text-xs text-ink-2 hover:text-ink" aria-label={"Your profile: " + name}>
        {agent ? null : <span className="dot" aria-hidden="true" />}
        <span className="min-w-0 truncate">{agent && me.ownerId ? name + ", for " + me.ownerId : name}</span>
      </Link>
      {canSwitchMode(me.kind) ? <ModeSwitch mode={mode} /> : null}
      <ThemeSwitch />
    </div>
  );
}

function Drawer({ props }: { props: AppShellProps }) {
  const [open, setOpen] = useState(false);
  const close = () => setOpen(false);
  return (
    <Dialog open={open} onOpenChange={(v) => setOpen(v)}>
      <DialogTrigger className={buttonClass("ghost", "sm", "size-10 px-0")} aria-label="Open navigation">
        <Icon name="menu" size={22} />
      </DialogTrigger>
      <DialogBackdrop />
      <DialogContent className="fixed inset-y-0 right-auto left-0 m-0 flex h-dvh max-h-none w-[min(320px,88vw)] flex-col overflow-y-auto border-r border-line bg-bg px-[18px] py-4 text-ink shadow-overlay outline-none scrollbar-thin">
        <DialogTitle className="sr-only">Navigation</DialogTitle>
        <div className="mb-4 flex items-center justify-between">
          <Wordmark onClick={close} />
          <DialogClose className={buttonClass("ghost", "sm", "size-10 px-0")} aria-label="Close navigation">
            <Icon name="x" size={20} />
          </DialogClose>
        </div>
        <RailBody props={props} onNavigate={close} drawer />
        {props.me ? <RailFoot me={props.me} mode={props.mode} onNavigate={close} /> : null}
      </DialogContent>
    </Dialog>
  );
}

function SkipLink() {
  return (
    <a href="#main" className="sr-only z-50 rounded-md bg-surface px-3 py-2 underline focus:not-sr-only focus:fixed focus:top-2 focus:left-2">
      Skip to content
    </a>
  );
}

function PaletteButton() {
  return (
    <button type="button" onClick={openPalette} className={buttonClass("ghost", "sm", "size-9 px-0")} aria-label="Command palette" title="Command palette (⌘K)">
      <Icon name="search" size={18} />
    </button>
  );
}

/** Remembers the repo a repo page is under, for the legacy root-level redirects. */
function useRememberRepo(fullName: string | undefined) {
  useEffect(() => {
    if (fullName) setRepoCookie(fullName);
  }, [fullName]);
}

/** The page column: generous margins that scale with the window, and room below the last line. */
const MAIN = "min-w-0 px-5 pt-8 pb-24 sm:px-8 md:px-[5vw] md:pt-[7vh] md:pb-[14vh]";

/** The agent's header, in the agent's view only. */
function Page({ props }: { props: AppShellProps }) {
  const me = props.me;
  const agent = me?.kind === "PRINCIPAL_KIND_AGENT";
  return (
    <main id="main" className={MAIN}>
      <div className="max-w-[1120px]">
        {props.mode === "bee" ? <BeeHeader reader={{ agent: agent && me ? me.display || me.id : null, answersTo: agent && me?.ownerId ? me.ownerId : undefined }} /> : null}
        {props.children}
      </div>
    </main>
  );
}

/* ---------------------------------------------------------------- the shell */

export function AppShell(props: AppShellProps) {
  useRememberRepo(props.repo?.fullName);
  const chrome = <Chrome {...props.chrome} repo={props.repo?.fullName ?? props.chrome.repo} canWrite={props.repo ? props.repo.canWrite : props.chrome.canWrite} signedIn={!!props.me} />;
  return props.me ? (
    <SignedIn props={props} me={props.me}>
      {chrome}
    </SignedIn>
  ) : (
    <SignedOut props={props}>{chrome}</SignedOut>
  );
}

function SignedIn({ props, me, children }: { props: AppShellProps; me: Principal; children: ReactNode }) {
  return (
    <div className="min-h-dvh md:grid md:grid-cols-[228px_minmax(0,1fr)]">
      <SkipLink />
      {/* Below md: a slim bar with the drawer. */}
      <header className="sticky top-0 z-30 flex h-14 items-center gap-1 border-b border-line bg-bg px-2 md:hidden">
        <Drawer props={props} />
        <Wordmark />
        <div className="ml-auto flex items-center gap-1">
          <PaletteButton />
        </div>
      </header>
      <aside aria-label="Beehive" className="hidden border-r border-line md:block">
        <div className="sticky top-0 flex h-dvh flex-col overflow-y-auto px-[18px] py-[26px] scrollbar-thin">
          <div className="mb-[18px] flex items-center justify-between">
            <Wordmark />
            <PaletteButton />
          </div>
          <RailBody props={props} />
          <RailFoot me={me} mode={props.mode} />
        </div>
      </aside>
      <Page props={props} />
      {children}
    </div>
  );
}

function SignedOut({ props, children }: { props: AppShellProps; children: ReactNode }) {
  const path = usePathname();
  // Pricing folds away on phones; the landing links to it too.
  const links: { href: string; label: string; wide?: boolean }[] = [{ href: "/explore", label: "Explore" }];
  if (props.edition === "cloud") links.push({ href: "/pricing", label: "Pricing", wide: true });
  const on = (href: string) => path === href || path.startsWith(href + "/");
  return (
    <div className="min-h-dvh">
      <SkipLink />
      <header className="sticky top-0 z-30 border-b border-line bg-bg">
        <div className="flex h-16 items-center gap-2 px-3 sm:gap-4 sm:px-8 md:px-[5vw]">
          {props.repo ? (
            <span className="md:hidden">
              <Drawer props={props} />
            </span>
          ) : null}
          <Wordmark />
          <nav aria-label="Main" className="flex items-center gap-1">
            {links.map((l) => (
              <Link key={l.href} href={l.href} aria-current={on(l.href) ? "page" : undefined} className={cx("h-9 items-center rounded-[10px] px-2.5 text-base transition-colors duration-[160ms]", l.wide ? "hidden sm:inline-flex" : "inline-flex", on(l.href) ? "bg-selected text-ink" : "text-ink-2 hover:text-ink")}>
                {l.label}
              </Link>
            ))}
          </nav>
          <div className="ml-auto flex min-w-0 items-center justify-end gap-1.5 sm:gap-2.5">
            <SearchField id="global-search" action="/repositories" label="Search repositories" placeholder="Search repositories" className="hidden w-64 lg:block" />
            <Link href="/repositories" className={buttonClass("ghost", "sm", "size-9 px-0 lg:hidden")} aria-label="Search repositories">
              <Icon name="search" size={18} />
            </Link>
            {/* Signed out there is no rail: someone previewing the agent's view switches back here. */}
            {props.mode === "bee" ? <ModeSwitch mode={props.mode} className="w-36" /> : null}
            <ThemeSwitch className="hidden w-48 sm:flex" />
            <Link href={"/signin" + (path && path !== "/" ? "?next=" + encodeURIComponent(path) : "")} className={buttonClass("primary", "sm")}>
              Sign in
            </Link>
          </div>
        </div>
      </header>
      <Page props={props} />
      {children}
    </div>
  );
}
