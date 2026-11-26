/**
 * The quiet end of a repository's home: who is editing right now, and what the repository is
 * (topics, languages, licence, homepage, a source archive). Two sections under the README, the
 * files and the commits; there is no side column any more.
 */
import Link from "next/link";
import { Kv, Quiet, Sect } from "@/components/parts";
import type { Lease, LanguageStat, Principal, Repo, RepoWorkItem } from "@/lib/api/types";
import { plural } from "@/lib/format";
import { ArchiveDownload } from "./archive-download";
import { LanguageBar } from "./language-bar";
import { activityHref, editingSentence } from "./presence";

/** An active agent the viewer may see; anonymous visitors see leases only. */
export interface ActiveAgent {
  principal: Principal;
  openChanges: number;
  activeLeases: number;
}

/** Who is editing, one sentence each. Agents that are active but hold no lease are named too. */
export function editingLines(leases: Lease[], agents: ActiveAgent[]): string[] {
  return editingMentions(leases, agents).map(({ line }) => line);
}

export function editingMentions(leases: Lease[], agents: ActiveAgent[]) {
  const holding = new Set(leases.map((l) => l.holder?.id));
  return [
    ...leases.map((lease) => ({ id: lease.id, line: editingSentence([lease]), principal: lease.holder, lease })),
    ...agents.filter((a) => !holding.has(a.principal.id)).map((a) => ({
      id: a.principal.id,
      line: (a.principal.display || a.principal.id) + " is active, with " + plural(a.openChanges, "open pull request") + ".",
      principal: a.principal,
      lease: undefined,
    })),
  ];
}

export function RepoNote({ repo, languages, leases, leasesVisible, agents, signedIn, work = [] }: { repo: Repo; languages: LanguageStat[]; leases: Lease[]; leasesVisible: boolean; agents: ActiveAgent[]; signedIn: boolean; work?: RepoWorkItem[] }) {
  const editing = editingMentions(leases, agents);
  return (
    <>
      <Sect id="editing-heading" title="Editing now">
        {editing.length ? (
          <ul className="flex flex-col gap-1.5 text-md">
            {editing.map(({ id, line, principal, lease }) => (
              <li key={id} className="[overflow-wrap:anywhere]">
                <Link className="link inline-flex min-h-6 items-center" href={activityHref(repo.fullName, work, signedIn ? principal : undefined, lease)}>{line}</Link>
              </li>
            ))}
          </ul>
        ) : (
          <Quiet>{leasesVisible || signedIn ? "Nobody is editing right now." : "Sign in to see who is editing here."}</Quiet>
        )}
      </Sect>
      <Sect id="about-heading" title="About">
        <div className="flex flex-col gap-5">
          {languages.length ? <LanguageBar languages={languages} /> : null}
          <Kv
            rows={[
              repo.topics.length
                ? [
                    "Topics",
                    <ul key="topics" className="flex flex-wrap gap-x-4 gap-y-1">
                      {repo.topics.map((t) => (
                        <li key={t}>
                          <Link href={"/topics/" + encodeURIComponent(t)} className="link">
                            {t}
                          </Link>
                        </li>
                      ))}
                    </ul>,
                  ]
                : null,
              ["License", repo.licenseSpdx || "None detected"],
              repo.homepage
                ? [
                    "Homepage",
                    <a key="homepage" href={repo.homepage} rel="noreferrer noopener" target="_blank" className="link">
                      {repo.homepage.replace(/^https?:\/\//, "")}
                    </a>,
                  ]
                : null,
              ["Download", <ArchiveDownload key="download" fullName={repo.fullName} />],
            ]}
          />
        </div>
      </Sect>
    </>
  );
}
