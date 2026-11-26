"use client";

/**
 * ⌘K, on UI SDK's Command inside the kit's dialog. Everything the chrome links to is here, and
 * so is everything it does not link to (Explore, Trending, Leaderboards, Search, Import, New,
 * the organisation's settings, signing out), so no page is reachable only by typing its URL.
 */
import { Command, CommandEmpty, CommandGroup, CommandGroupLabel, CommandInput, CommandItem, CommandList, CommandPage } from "@ui-sdk/react/command";
import { usePathname, useRouter } from "next/navigation";
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { browserClient } from "@/lib/api/browser";
import type { AgentSummary, ChangeRequest, Issue, Run } from "@/lib/api/types";
import { classInfo } from "@/lib/diffclass";
import { shortId } from "@/lib/format";
import { navHref, orgRailItems, paletteExtras, railItems, repoMore, repoTabs, type NavItem } from "@/lib/nav";
import { orgSettingsHref, repoHref } from "@/lib/routes";
import { Icon, type IconName } from "./icons";
import { Modal } from "./kit/dialog";
import { setRepoCookie, switchTarget } from "./repo-picker";
import { toggleTheme } from "./theme";
import { Kbd } from "./ui";

interface Dyn {
  changes: ChangeRequest[];
  agents: AgentSummary[];
  issues: Issue[];
  runs: Run[];
}

export function openPalette() {
  window.dispatchEvent(new Event("bee-palette"));
}

/** Pages with no nav item of their own; the rail's and the organisation's come first (lib/nav.ts). */
const DISCOVER: { href: string; label: string; icon: IconName; blurb: string; signedIn?: boolean }[] = [
  { href: "/trending", label: "Trending", icon: "trend", blurb: "What is gaining stars and merges this week" },
  { href: "/leaderboards", label: "Leaderboards", icon: "trophy", blurb: "Top agents, people and repositories" },
  { href: "/new", label: "New", icon: "plus", blurb: "Create a repository or an organization", signedIn: true },
];

const REPOS_PAGE = "repos";

/**
 * `repo` is the repo the page is under (null on global pages): its sections, pull requests,
 * agents, issues and runs are listed when there is one.
 */
export function CommandPalette({ repo, repos, org, canWrite = true, signedIn = true }: { repo: string | null; repos: string[]; org: string; canWrite?: boolean; signedIn?: boolean }) {
  const router = useRouter();
  const path = usePathname();
  const [open, setOpen] = useState(false);
  const [pages, setPages] = useState<string[]>([]);
  const [dyn, setDyn] = useState<Dyn | null>(null);
  const [search, setSearch] = useState("");

  useEffect(() => {
    const reset = () => {
      setPages([]);
      setSearch("");
    };
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        reset();
        setOpen((v) => !v);
      }
    };
    const onOpen = () => {
      reset();
      setOpen(true);
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("bee-palette", onOpen);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("bee-palette", onOpen);
    };
  }, []);

  useEffect(() => {
    if (!open || !repo) return;
    let alive = true;
    const api = browserClient();
    const r = { fullName: repo };
    Promise.all([
      api.change.listChanges({ repo: r, state: "CHANGE_STATE_OPEN" }).then((x) => x.changes),
      api.agent.listAgents({ org, repo: r }).then((x) => x.agents),
      api.issue.listIssues({ repo: r, state: "open" }).then((x) => x.issues),
      api.workflow.listRuns({ repo: r, page: { pageSize: 8 } }).then((x) => x.runs),
    ])
      .then(([changes, agents, issues, runs]) => alive && setDyn({ changes, agents, issues, runs }))
      .catch(() => alive && setDyn({ changes: [], agents: [], issues: [], runs: [] }));
    return () => {
      alive = false;
    };
  }, [open, repo, org]);

  const go = useCallback(
    (href: string) => {
      setOpen(false);
      router.push(href);
    },
    [router],
  );

  const signOut = async () => {
    setOpen(false);
    await fetch("/api/session", { method: "DELETE" });
    router.push("/signin");
    router.refresh();
  };

  const scope = repo ?? org + "/";
  const navItem = (n: NavItem) => (
    <Item key={n.id} id={"go-" + n.id} icon={n.icon} onSelect={() => go(navHref(n, scope, org))} words={n.label + " " + n.blurb} hint={"g " + n.key}>
      {n.label}
      <span className="ml-2 truncate text-ink-3">{n.blurb}</span>
    </Item>
  );
  const onRepos = pages.at(-1) === REPOS_PAGE;

  return (
    <Modal open={open} onOpenChange={setOpen} title="Command palette" hideTitle className="top-[10vh] w-[min(640px,calc(100vw-20px))] overflow-hidden p-0">
      <Command loop query={search} onQueryChange={(q) => setSearch(q)} pages={pages} onPagesChange={(p) => setPages(p)} className="flex max-h-[min(560px,70vh)] flex-col">
        <div className="flex items-center gap-2 border-b border-line px-4">
          <Icon name="search" className="text-ink-3" />
          <CommandInput aria-label="Command palette" placeholder={onRepos ? "Find a repository…" : "Jump to a pull request, agent, run, issue or action…"} className="h-[52px] min-w-0 flex-1 bg-transparent text-md text-ink outline-none placeholder:text-ink-3" />
          <Kbd>esc</Kbd>
        </div>
        <CommandEmpty className="px-3 py-8 text-center text-sm text-ink-2">Nothing matches. Try a path, a title or an agent name.</CommandEmpty>
        <CommandList className="overflow-y-auto p-1.5 scrollbar-thin">
          <CommandPage>
            {repo ? <Group heading={"In " + repo}>{[...repoTabs(canWrite), ...repoMore()].map(navItem)}</Group> : null}
            <Group heading="Go to">
              {railItems(signedIn).map(navItem)}
              {orgRailItems(signedIn, org).map(navItem)}
              {paletteExtras(signedIn, org).map(navItem)}
              {DISCOVER.filter((g) => signedIn || !g.signedIn).map((g) => (
                <Item key={g.href} id={"go-" + g.href} icon={g.icon} onSelect={() => go(g.href)} words={g.label + " " + g.blurb}>
                  {g.label}
                  <span className="ml-2 truncate text-ink-3">{g.blurb}</span>
                </Item>
              ))}
              {repos.length ? (
                <Item id="go-repo" icon="repo" page={REPOS_PAGE} words="Go to repo repository switch">
                  Go to repo…
                  <span className="ml-2 text-ink-3">Pick one of {repos.length === 1 ? "1 repository" : repos.length + " repositories"}</span>
                </Item>
              ) : null}
            </Group>
            {repo && dyn?.changes.length ? (
              <Group heading="Open pull requests">
                {dyn.changes.map((c) => (
                  <Item key={c.id} id={"change-" + c.id} icon="changes" onSelect={() => go(repoHref(repo, "changes/" + c.id))} words={c.title + " pull request change " + c.id + " " + (c.author?.display ?? "")}>
                    <span className="truncate">{c.title}</span>
                    <span className="ml-2 shrink-0 text-ink-3">{classInfo(c.revisions.at(-1)?.diffClass ?? "").label}</span>
                  </Item>
                ))}
              </Group>
            ) : null}
            {repo && dyn?.agents.length ? (
              <Group heading="Agents">
                {dyn.agents.map((a) => (
                  <Item key={a.principal.id} id={"agent-" + a.principal.id} icon="agents" onSelect={() => go(repoHref(repo, "agents/" + a.principal.id))} words={a.principal.display + " agent " + a.principal.runtime}>
                    {a.principal.display}
                    <span className="ml-2 text-sm text-ink-3">{a.principal.runtime}</span>
                  </Item>
                ))}
              </Group>
            ) : null}
            {repo && dyn?.issues.length ? (
              <Group heading="Open issues">
                {dyn.issues.slice(0, 8).map((i) => (
                  <Item key={i.id} id={"issue-" + i.id} icon="work" onSelect={() => go(repoHref(repo, "work/" + i.number))} words={i.title + " issue " + i.number}>
                    <span className="num mr-2 text-ink-3">#{i.number}</span>
                    <span className="truncate">{i.title}</span>
                  </Item>
                ))}
              </Group>
            ) : null}
            {repo && dyn?.runs.length ? (
              <Group heading="Recent CI runs">
                {dyn.runs.map((r) => (
                  <Item key={r.id} id={"run-" + r.id} icon="runs" onSelect={() => go(repoHref(repo, "runs/" + r.id))} words={r.workflow + " " + r.number + " run ci " + r.status + " " + shortId(r.id)}>
                    {r.workflow} #{r.number}
                    <span className="ml-2 text-ink-3">{r.status}</span>
                  </Item>
                ))}
              </Group>
            ) : null}
            <Group heading="Actions">
              <Item
                id="act-theme"
                icon="moon"
                onSelect={() => {
                  toggleTheme();
                  setOpen(false);
                }}
                words="Toggle light and dark theme"
              >
                Toggle light and dark theme
              </Item>
              {repo ? (
                <Item id="act-mint" icon="key" onSelect={() => go(repoHref(repo, "agents"))} words="Mint a narrowed token for an agent scope budget">
                  Mint a narrowed token for an agent
                </Item>
              ) : null}
              {repo && canWrite ? (
                <Item id="act-policy" icon="settings" onSelect={() => go(repoHref(repo, "settings"))} words="Evaluate a policy change ruleset merge rules">
                  Evaluate a policy change
                </Item>
              ) : null}
              {signedIn ? (
                <Item id="act-import" icon="import" onSelect={() => go("/import")} words="Import from GitHub repository migrate">
                  Import from GitHub
                </Item>
              ) : null}
              {signedIn && org ? (
                <Item id="act-audit" icon="lock" onSelect={() => go(orgSettingsHref(org, "audit"))} words="Search the audit log record who changed">
                  Search the audit log
                </Item>
              ) : null}
              {repos
                .filter((r) => r !== repo)
                .map((r) => (
                  <Item
                    key={r}
                    id={"switch-" + r}
                    icon="branch"
                    onSelect={() => {
                      setRepoCookie(r);
                      go(switchTarget(path, r));
                    }}
                    words={"Switch to " + r + " repository"}
                  >
                    Switch to {r}
                  </Item>
                ))}
              {signedIn ? (
                <Item id="act-sign-out" icon="signOut" onSelect={() => void signOut()} words="Sign out log out">
                  Sign out
                </Item>
              ) : null}
            </Group>
          </CommandPage>
          <CommandPage value={REPOS_PAGE}>
            <Group heading="Go to repository">
              {repos.map((r) => (
                <Item key={r} id={"repo-" + r} icon="repo" onSelect={() => go(repoHref(r))} words={r + " repo"}>
                  <span className="truncate">{r}</span>
                </Item>
              ))}
            </Group>
          </CommandPage>
        </CommandList>
      </Command>
    </Modal>
  );
}

function Group({ heading, children }: { heading: string; children: ReactNode }) {
  return (
    <CommandGroup className="mb-1">
      <CommandGroupLabel className="px-2.5 pt-2 pb-1 text-2xs font-medium text-ink-3">{heading}</CommandGroupLabel>
      {children}
    </CommandGroup>
  );
}

/**
 * One command. `id` is its value, unique in the list. `words` is everything the search matches:
 * the visible label first (it is also the accessible name), then synonyms.
 */
function Item({ id, icon, children, onSelect, page, words, hint }: { id: string; icon: IconName; children: ReactNode; onSelect?: () => void; page?: string; words: string; hint?: string }) {
  return (
    <CommandItem value={id} label={words} page={page} onSelect={onSelect ? () => onSelect() : undefined} className="flex min-h-10 cursor-pointer items-center gap-3 rounded-md px-3 text-base text-ink data-[highlighted]:bg-hover">
      <Icon name={icon} size={18} className="shrink-0 text-ink-3" />
      <span className="flex min-w-0 flex-1 items-center truncate">{children}</span>
      {hint ? (
        <span className="flex gap-1" aria-hidden="true">
          {hint.split(" ").map((k) => (
            <Kbd key={k}>{k}</Kbd>
          ))}
        </span>
      ) : null}
    </CommandItem>
  );
}
