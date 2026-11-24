"use client";

import * as Dialog from "@radix-ui/react-dialog";
import { Command } from "cmdk";
import { usePathname, useRouter } from "next/navigation";
import { useCallback, useEffect, useState } from "react";
import { browserClient } from "@/lib/api/browser";
import type { AgentSummary, ChangeRequest, Issue, Run } from "@/lib/api/types";
import { classInfo } from "@/lib/diffclass";
import { shortId } from "@/lib/format";
import { NAV, navHref, railItems, repoMore, repoTabs } from "@/lib/nav";
import { orgSettingsHref, repoHref } from "@/lib/routes";
import { Icon, type IconName } from "./icons";
import { toggleTheme } from "./theme";
import { Kbd } from "./ui";
import { setRepoCookie, switchTarget } from "./repo-picker";

interface Dyn {
  changes: ChangeRequest[];
  agents: AgentSummary[];
  issues: Issue[];
  runs: Run[];
}

export function openPalette() {
  window.dispatchEvent(new Event("bee-palette"));
}

/** Discovery pages that are not in the rail; the rail's own items come first (lib/nav.ts). */
const DISCOVER: { href: string; label: string; icon: IconName; blurb: string }[] = [
  { href: "/trending", label: "Trending", icon: "trend", blurb: "What is gaining stars and lands this week" },
  { href: "/leaderboards", label: "Leaderboards", icon: "trophy", blurb: "Top agents, people and repositories" },
  { href: "/new", label: "New", icon: "plus", blurb: "Create a repository or an organization" },
];

/**
 * ⌘K. `repo` is the repo the page is under (null on global pages): its sections, changes,
 * agents, issues and runs are listed when there is one.
 */
export function CommandPalette({ repo, repos, org, canWrite = true, signedIn = true }: { repo: string | null; repos: string[]; org: string; canWrite?: boolean; signedIn?: boolean }) {
  const router = useRouter();
  const path = usePathname();
  const [open, setOpen] = useState(false);
  const [page, setPage] = useState<"root" | "repos">("root");
  const [dyn, setDyn] = useState<Dyn | null>(null);
  const [search, setSearch] = useState("");

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPage("root");
        setSearch("");
        setOpen((v) => !v);
      }
    };
    const onOpen = () => {
      setPage("root");
      setSearch("");
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

  const showPage = (p: "root" | "repos") => {
    setPage(p);
    setSearch("");
  };

  const go = useCallback(
    (href: string) => {
      setOpen(false);
      router.push(href);
    },
    [router],
  );

  return (
    <Dialog.Root open={open} onOpenChange={setOpen}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/50" />
        <Dialog.Content
          className="fixed top-[10vh] left-1/2 z-50 w-[min(640px,calc(100vw-20px))] -translate-x-1/2 overflow-hidden rounded-lg border border-line bg-popover shadow-overlay data-[state=open]:animate-enter"
          aria-describedby={undefined}
        >
          <Dialog.Title className="sr-only">Command palette</Dialog.Title>
          <Command
            label="Command palette"
            loop
            className="flex max-h-[min(560px,70vh)] flex-col"
            onKeyDown={(e) => {
              // Backspace on an empty query steps back out of the repository list.
              if (page === "repos" && e.key === "Backspace" && !search) {
                e.preventDefault();
                showPage("root");
              }
            }}
          >
            <div className="flex items-center gap-2 border-b border-line px-3.5">
              <Icon name="search" className="text-ink-3" />
              <Command.Input autoFocus value={search} onValueChange={setSearch} placeholder={page === "repos" ? "Find a repository…" : "Jump to a change, agent, run, issue or action…"} className="h-12 flex-1 bg-transparent text-md outline-none placeholder:text-ink-3" />
              <Kbd>esc</Kbd>
            </div>
            <Command.List className="overflow-y-auto p-1.5 scrollbar-thin">
              <Command.Empty className="px-3 py-8 text-center text-sm text-ink-2">Nothing matches. Try a path, a title or an agent name.</Command.Empty>
              {page === "repos" ? (
                <Group heading="Go to repository">
                  {repos.map((r) => (
                    <Item key={r} icon="repo" onSelect={() => go(repoHref(r))} value={"repo " + r}>
                      <span className="truncate">{r}</span>
                    </Item>
                  ))}
                </Group>
              ) : (
                <>
                  {repo ? (
                    <Group heading={"In " + repo}>
                      {[...repoTabs(), ...repoMore()].filter((n) => canWrite || !n.write).map((n) => (
                        <Item key={n.id} icon={n.icon} onSelect={() => go(navHref(n, repo, org))} value={"go " + n.label + " " + n.blurb} hint={"g " + n.key}>
                          {n.label}
                          <span className="ml-2 text-ink-3">{n.blurb}</span>
                        </Item>
                      ))}
                    </Group>
                  ) : null}
                  <Group heading="Go to">
                    {railItems(signedIn)
                      .filter((n) => n.target.scope === "global" || !repo)
                      .map((n) => (
                        <Item key={n.id} icon={n.icon} onSelect={() => go(navHref(n, repo ?? org + "/", org))} value={"go " + n.label + " " + n.blurb} hint={"g " + n.key}>
                          {n.label}
                          <span className="ml-2 text-ink-3">{n.blurb}</span>
                        </Item>
                      ))}
                    {DISCOVER.map((g) => (
                      <Item key={g.href} icon={g.icon} onSelect={() => go(g.href)} value={"go " + g.label + " " + g.blurb}>
                        {g.label}
                        <span className="ml-2 text-ink-3">{g.blurb}</span>
                      </Item>
                    ))}
                    {repos.length ? (
                      <Item icon="repo" onSelect={() => showPage("repos")} value="go to repo repository switch">
                        Go to repo…
                        <span className="ml-2 text-ink-3">Pick one of {repos.length === 1 ? "1 repository" : repos.length + " repositories"}</span>
                      </Item>
                    ) : null}
                    {NAV.filter((n) => n.target.scope !== "repo" && !railItems(true).includes(n)).map((n) => (
                      <Item key={n.id} icon={n.icon} onSelect={() => go(navHref(n, repo ?? org + "/", org))} value={"go " + n.label + " " + n.blurb} hint={"g " + n.key}>
                        {n.label}
                        <span className="ml-2 text-ink-3">{n.blurb}</span>
                      </Item>
                    ))}
                  </Group>
                  {repo && dyn?.changes.length ? (
                    <Group heading="Open change requests">
                      {dyn.changes.map((c) => (
                        <Item key={c.id} icon="changes" onSelect={() => go(repoHref(repo, "changes/" + c.id))} value={"change " + c.title + " " + c.id + " " + (c.author?.display ?? "")}>
                          <span className="truncate">{c.title}</span>
                          <span className="ml-2 shrink-0 text-ink-3">{classInfo(c.revisions.at(-1)?.diffClass ?? "").label}</span>
                        </Item>
                      ))}
                    </Group>
                  ) : null}
                  {repo && dyn?.agents.length ? (
                    <Group heading="Agents">
                      {dyn.agents.map((a) => (
                        <Item key={a.principal.id} icon="agents" onSelect={() => go(repoHref(repo, "agents/" + a.principal.id))} value={"agent " + a.principal.display + " " + a.principal.runtime}>
                          {a.principal.display}
                          <span className="ml-2 text-sm text-ink-3">{a.principal.runtime}</span>
                        </Item>
                      ))}
                    </Group>
                  ) : null}
                  {repo && dyn?.issues.length ? (
                    <Group heading="Open issues">
                      {dyn.issues.slice(0, 8).map((i) => (
                        <Item key={i.id} icon="work" onSelect={() => go(repoHref(repo, "work/" + i.number))} value={"issue " + i.number + " " + i.title}>
                          <span className="num mr-2 text-ink-3">#{i.number}</span>
                          <span className="truncate">{i.title}</span>
                        </Item>
                      ))}
                    </Group>
                  ) : null}
                  {repo && dyn?.runs.length ? (
                    <Group heading="Recent runs">
                      {dyn.runs.map((r) => (
                        <Item key={r.id} icon="runs" onSelect={() => go(repoHref(repo, "runs/" + r.id))} value={"run " + r.workflow + " " + r.number + " " + r.status + " " + shortId(r.id)}>
                          {r.workflow} #{r.number}
                          <span className="ml-2 text-ink-3">{r.status}</span>
                        </Item>
                      ))}
                    </Group>
                  ) : null}
                  <Group heading="Actions">
                    <Item icon="moon" onSelect={() => { toggleTheme(); setOpen(false); }} value="toggle theme dark light">
                      Toggle light and dark theme
                    </Item>
                    {repo ? (
                      <Item icon="key" onSelect={() => go(repoHref(repo, "agents"))} value="mint token agent scope budget">
                        Mint a narrowed token for an agent
                      </Item>
                    ) : null}
                    {repo && canWrite ? (
                      <Item icon="settings" onSelect={() => go(repoHref(repo, "settings"))} value="evaluate policy ruleset land rules">
                        Evaluate a policy change
                      </Item>
                    ) : null}
                    <Item icon="import" onSelect={() => go("/import")} value="import github repository migrate">
                      Import from GitHub
                    </Item>
                    {org ? (
                      <Item icon="lock" onSelect={() => go(orgSettingsHref(org, "audit"))} value="audit log who changed">
                        Search the audit log
                      </Item>
                    ) : null}
                    {repos
                      .filter((r) => r !== repo)
                      .map((r) => (
                        <Item key={r} icon="branch" onSelect={() => { setRepoCookie(r); go(switchTarget(path, r)); }} value={"switch repository " + r}>
                          Switch to {r}
                        </Item>
                      ))}
                  </Group>
                </>
              )}
            </Command.List>
          </Command>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function Group({ heading, children }: { heading: string; children: React.ReactNode }) {
  return (
    <Command.Group heading={heading} className="mb-1 [&_[cmdk-group-heading]]:px-2.5 [&_[cmdk-group-heading]]:pt-2 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:text-xs [&_[cmdk-group-heading]]:font-medium [&_[cmdk-group-heading]]:text-ink-3">
      {children}
    </Command.Group>
  );
}

function Item({ icon, children, onSelect, value, hint }: { icon: IconName; children: React.ReactNode; onSelect: () => void; value: string; hint?: string }) {
  return (
    <Command.Item value={value} onSelect={onSelect} className="flex h-10 cursor-pointer items-center gap-3 rounded-md px-3 text-base text-ink data-[selected=true]:bg-hover">
      <Icon name={icon} size={18} className="shrink-0 text-ink-3" />
      <span className="flex min-w-0 flex-1 items-center truncate">{children}</span>
      {hint ? (
        <span className="flex gap-1">
          {hint.split(" ").map((k) => (
            <Kbd key={k}>{k}</Kbd>
          ))}
        </span>
      ) : null}
    </Command.Item>
  );
}
