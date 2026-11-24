"use client";

/**
 * The app shell: the one navigation system on every page (docs/spec/web-ui.md §2).
 *
 *   signed in   a persistent left rail: the wordmark, search, Inbox (with what needs you), Feed,
 *               Agents, Repositories and Explore; the current repository on repo pages; New,
 *               theme and you (with your org) at the bottom. Below `md` the rail is a drawer
 *               behind a slim top bar.
 *   signed out  a slim top bar: the wordmark, Explore (and Pricing on cloud), search, Sign in.
 *
 * It also mounts the keyboard chrome once: the `g` chords, `?`, `/`, j/k, `y` Copy-as and ⌘K.
 * Pages render inside `<main id="main">` and choose their own measure (see docs/design/tokens.md).
 */
import * as Dialog from "@radix-ui/react-dialog";
import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import { useEffect, useState, type ReactNode } from "react";
import type { Principal } from "@/lib/api/types";
import { NAV, SHORTCUTS, isActive, navHref, railItems, repoMore, repoTabs, type NavItem } from "@/lib/nav";
import { isRepoFullName, orgSettingsHref, ownerHref } from "@/lib/routes";
import { Avatar, toAvatar } from "./avatar";
import { CommandPalette, openPalette } from "./command-palette";
import { GlobalCopyAs } from "./copy-as";
import { MENU_ITEM, Menu, MenuLink } from "./menu";
import { Icon } from "./icons";
import { RepoPicker, setRepoCookie } from "./repo-picker";
import { ThemeToggle } from "./theme";
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

/** The repository a repo page is under, for the rail's switcher and the drawer's sections. */
export interface ShellRepo {
  fullName: string;
  canWrite: boolean;
}

export interface AppShellProps {
  me: Principal | null;
  edition: "cloud" | "enterprise";
  adapter: "http" | "fixture";
  /** The org the account menu's settings link opens and the rail names under you. */
  org: string;
  /**
   * The Inbox count, rendered beside "Inbox". A server component in a Suspense boundary
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
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/50" />
        <Dialog.Content className="fixed top-[8vh] left-1/2 z-50 max-h-[84vh] w-[min(600px,calc(100vw-24px))] -translate-x-1/2 overflow-y-auto rounded-xl border border-line bg-popover p-5 shadow-overlay data-[state=open]:animate-enter" aria-describedby={undefined}>
          <div className="mb-4 flex items-center justify-between">
            <Dialog.Title className="text-xl font-semibold tracking-tight">Keyboard shortcuts</Dialog.Title>
            <Dialog.Close className={buttonClass("ghost", "sm", "size-8 px-0")} aria-label="Close">
              <Icon name="x" size={18} />
            </Dialog.Close>
          </div>
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
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
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
      if (e.metaKey || e.ctrlKey || e.altKey || isTyping(e.target) || document.querySelector("[role=dialog],[role=menu]")) return;
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

/* ---------------------------------------------------------------- menus */

function SignOutItem() {
  const router = useRouter();
  return (
    <button
      type="button"
      role="menuitem"
      tabIndex={-1}
      className={MENU_ITEM}
      onClick={async () => {
        await fetch("/api/session", { method: "DELETE" });
        router.push("/signin");
        router.refresh();
      }}
    >
      <Icon name="signOut" size={18} className="text-ink-3" />
      Sign out
    </button>
  );
}

/** You and your org at the bottom of the rail; opens your profile, repositories, settings, sign out. */
function AccountMenu({ me, org, adapter, compact = false }: { me: Principal; org: string; adapter: AppShellProps["adapter"]; compact?: boolean }) {
  const name = me.display || me.id;
  return (
    <Menu
      label={"Account menu for " + name}
      place={compact ? "below-end" : "above-start"}
      triggerClass={compact ? "grid size-10 place-items-center rounded-full hover:bg-hover" : "flex h-14 w-full items-center gap-3 rounded-lg px-2.5 text-left transition-colors duration-[120ms] hover:bg-hover"}
      trigger={
        <>
          <span className="sr-only">Account menu: </span>
          <Avatar principal={toAvatar(me)} size={compact ? 30 : 32} />
          {compact ? null : (
            <span className="min-w-0 flex-1 leading-tight" aria-hidden="true">
              <span className="block truncate text-base font-medium text-ink">{name}</span>
              <span className="block truncate text-sm text-ink-3">{org || (adapter === "fixture" ? "Demo data" : "Personal")}</span>
            </span>
          )}
        </>
      }
    >
      <div className="border-b border-line px-2.5 pt-1.5 pb-2.5">
        <p className="truncate text-base font-medium">{name}</p>
        <p className="truncate text-sm text-ink-3">{adapter === "fixture" ? "Demo data" : "Connected"}</p>
      </div>
      <div className="flex flex-col gap-px pt-1.5">
        <MenuLink href={ownerHref(name)} icon="user">
          Your profile
        </MenuLink>
        <MenuLink href="/repositories" icon="repo">
          Your repositories
        </MenuLink>
        {org ? (
          <MenuLink href={orgSettingsHref(org)} icon="team">
            Organization settings
          </MenuLink>
        ) : null}
        <MenuLink href="/import" icon="import">
          Import
        </MenuLink>
        <div className="my-1.5 border-t border-line" />
        <SignOutItem />
      </div>
    </Menu>
  );
}

function NewMenu() {
  return (
    <Menu label="Create new" labelled triggerClass={buttonClass("ghost", "sm", "size-9 px-0")} trigger={<Icon name="plus" size={18} />}>
      <MenuLink href="/new" icon="repo">
        New repository or organization
      </MenuLink>
      <MenuLink href="/import" icon="import">
        Import from GitHub
      </MenuLink>
    </Menu>
  );
}

/* ---------------------------------------------------------------- pieces */

/** The Beehive wordmark: a lattice drawn in the text colour, then the name. */
export function Wordmark({ onClick }: { onClick?: () => void }) {
  return (
    <Link href="/" onClick={onClick} aria-label="Beehive home" className="inline-flex h-10 shrink-0 items-center gap-2.5 rounded-md px-1.5 text-lg font-semibold tracking-tight text-ink">
      <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round">
        <path d="M4 19V3M11 19V3M18 19V3M4 8l7-5 7 5M4 14l7-5 7 5" />
      </svg>
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
      <Icon name="search" size={17} className="pointer-events-none absolute top-1/2 left-3 -translate-y-1/2 text-ink-3" />
      <input id={id} name="q" type="search" placeholder={placeholder} className="h-9 w-full rounded-full border border-transparent bg-input pr-3 pl-9 text-base text-ink placeholder:text-ink-3 transition-colors duration-[120ms] hover:border-line-strong focus:border-line-strong" />
    </form>
  );
}

const RAIL_ITEM = "flex h-10 items-center gap-3 rounded-lg px-2.5 text-base transition-colors duration-[120ms]";

function RailLink({ item, href, active, badge, onNavigate }: { item: NavItem; href: string; active: boolean; badge?: ReactNode; onNavigate?: () => void }) {
  return (
    <li>
      <Link href={href} onClick={onNavigate} aria-current={active ? "page" : undefined} className={cx(RAIL_ITEM, active ? "bg-selected font-medium text-ink" : "text-ink-2 hover:bg-hover hover:text-ink")}>
        <Icon name={item.icon} size={20} weight={active ? "fill" : "regular"} />
        <span className="flex-1 truncate">{item.label}</span>
        {badge}
      </Link>
    </li>
  );
}

/** The rail's body, shared by the desktop rail and the mobile drawer. */
function RailBody({ props, onNavigate, drawer = false }: { props: AppShellProps; onNavigate?: () => void; drawer?: boolean }) {
  const path = usePathname();
  const { me, repo, chrome, inboxBadge } = props;
  const scope = repo?.fullName ?? chrome.repo ?? props.org + "/";
  const pickable = repo ? (chrome.repos.includes(repo.fullName) ? chrome.repos : [repo.fullName, ...chrome.repos]) : [];
  return (
    <>
      <nav aria-label="Main">
        <ul className="flex flex-col gap-0.5">
          {railItems(!!me).map((item) => {
            // On a repo page the rail's Agents is that repo's agents; the repo's own tabs mark it.
            const active = item.id === "agents" && repo ? false : isActive(item, path, scope, props.org);
            return <RailLink key={item.id} item={item} href={navHref(item, scope, props.org)} active={active} badge={item.id === "inbox" ? inboxBadge : undefined} onNavigate={onNavigate} />;
          })}
        </ul>
      </nav>
      {repo ? (
        <section aria-label="Current repository" className="mt-8">
          <p className="px-2.5 pb-1.5 text-sm font-medium text-ink-3">Repository</p>
          <div className="px-1">
            <RepoPicker repo={repo.fullName} repos={pickable} id={drawer ? "repo-picker-drawer" : "repo-picker"} />
          </div>
          {drawer ? (
            <nav aria-label="Repository sections" className="mt-2">
              <ul className="flex flex-col gap-0.5">
                {[...repoTabs(), ...repoMore()]
                  .filter((n) => repo.canWrite || !n.write)
                  .map((n) => (
                    <RailLink key={n.id} item={n} href={navHref(n, repo.fullName)} active={isActive(n, path, repo.fullName)} onNavigate={onNavigate} />
                  ))}
              </ul>
            </nav>
          ) : null}
        </section>
      ) : null}
    </>
  );
}

function Drawer({ props }: { props: AppShellProps }) {
  const [open, setOpen] = useState(false);
  const close = () => setOpen(false);
  return (
    <Dialog.Root open={open} onOpenChange={setOpen}>
      <Dialog.Trigger className={buttonClass("ghost", "sm", "size-10 px-0")} aria-label="Open navigation">
        <Icon name="menu" size={22} />
      </Dialog.Trigger>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/50" />
        <Dialog.Content className="fixed inset-y-0 left-0 z-50 flex w-[min(320px,88vw)] flex-col overflow-y-auto bg-bg px-3 py-3 shadow-overlay scrollbar-thin data-[state=open]:animate-enter" aria-describedby={undefined}>
          <Dialog.Title className="sr-only">Navigation</Dialog.Title>
          <div className="mb-4 flex items-center justify-between">
            <Wordmark onClick={close} />
            <Dialog.Close className={buttonClass("ghost", "sm", "size-10 px-0")} aria-label="Close navigation">
              <Icon name="x" size={20} />
            </Dialog.Close>
          </div>
          {props.me ? <SearchField id="drawer-search" action="/search" label="Search code" placeholder="Search code" className="mb-4" /> : null}
          <RailBody props={props} onNavigate={close} drawer />
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function SkipLink() {
  return (
    <a href="#main" className="sr-only z-50 rounded-md bg-surface px-3 py-2 focus:not-sr-only focus:fixed focus:top-2 focus:left-2">
      Skip to content
    </a>
  );
}

function PaletteButton() {
  return (
    <button type="button" onClick={openPalette} className={buttonClass("ghost", "sm", "size-9 px-0")} aria-label="Command palette" title="Command palette (⌘K)">
      <Icon name="terminal" size={18} />
    </button>
  );
}

/** Remembers the repo a repo page is under, for the legacy root-level redirects. */
function useRememberRepo(fullName: string | undefined) {
  useEffect(() => {
    if (fullName) setRepoCookie(fullName);
  }, [fullName]);
}

const MAIN = "min-w-0 px-5 pt-8 pb-24 sm:px-8 lg:px-12 lg:pt-12";

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
    <div className="min-h-dvh md:grid md:grid-cols-[248px_minmax(0,1fr)]">
      <SkipLink />
      {/* Below md: a slim bar with the drawer. */}
      <header className="sticky top-0 z-30 flex h-14 items-center gap-1 border-b border-line bg-bg px-2 md:hidden">
        <Drawer props={props} />
        <Wordmark />
        <div className="ml-auto flex items-center gap-1">
          <PaletteButton />
          <AccountMenu me={me} org={props.org} adapter={props.adapter} compact />
        </div>
      </header>
      <aside aria-label="Beehive" className="hidden border-r border-line bg-sunken/50 md:block">
        <div className="sticky top-0 flex h-dvh flex-col overflow-y-auto px-3 pt-4 pb-3 scrollbar-thin">
          <div className="mb-4 flex items-center justify-between">
            <Wordmark />
            <PaletteButton />
          </div>
          <SearchField id="global-search" action="/search" label="Search code" placeholder="Search code" className="mb-5" />
          <RailBody props={props} />
          <div className="mt-auto pt-6">
            <div className="mb-1 flex items-center gap-1 px-1">
              <NewMenu />
              <ThemeToggle />
            </div>
            <AccountMenu me={me} org={props.org} adapter={props.adapter} />
          </div>
        </div>
      </aside>
      <main id="main" className={MAIN}>
        <div className="mx-auto max-w-[1120px]">{props.children}</div>
      </main>
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
      <header className="sticky top-0 z-30 bg-bg/95 backdrop-blur-sm">
        <div className="mx-auto flex h-16 max-w-[1240px] items-center gap-2 px-3 sm:gap-4 sm:px-6">
          {props.repo ? (
            <span className="md:hidden">
              <Drawer props={props} />
            </span>
          ) : null}
          <Wordmark />
          <nav aria-label="Main" className="flex items-center gap-1">
            {links.map((l) => (
              <Link key={l.href} href={l.href} aria-current={on(l.href) ? "page" : undefined} className={cx("h-9 items-center rounded-md px-2.5 text-base transition-colors duration-[120ms]", l.wide ? "hidden sm:inline-flex" : "inline-flex", on(l.href) ? "font-medium text-ink" : "text-ink-2 hover:text-ink")}>
                {l.label}
              </Link>
            ))}
          </nav>
          <div className="ml-auto flex min-w-0 items-center justify-end gap-1.5 sm:gap-2">
            <SearchField id="global-search" action="/repositories" label="Search repositories" placeholder="Search repositories" className="hidden w-72 md:block" />
            <Link href="/repositories" className={buttonClass("ghost", "sm", "size-9 px-0 md:hidden")} aria-label="Search repositories">
              <Icon name="search" size={18} />
            </Link>
            <ThemeToggle />
            <Link href={"/signin" + (path && path !== "/" ? "?next=" + encodeURIComponent(path) : "")} className={buttonClass("secondary", "sm")}>
              Sign in
            </Link>
          </div>
        </div>
      </header>
      <main id="main" className={MAIN}>
        <div className="mx-auto max-w-[1120px]">{props.children}</div>
      </main>
      {children}
    </div>
  );
}
