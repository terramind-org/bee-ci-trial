/**
 * The one quiet side note of a repo home. It replaces the old About, Working now and Recent
 * changes boxes: topics, licence, language mix, counts, who is editing right now, and where to
 * read more. Plain text under small labels; nothing is boxed.
 */
import Link from "next/link";
import { Avatar, toAvatar } from "@/components/avatar";
import { Icon } from "@/components/icons";
import type { Lease, LanguageStat, Principal, Repo } from "@/lib/api/types";
import { compact, plural } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { ArchiveDownload } from "./archive-download";
import { LanguageBar } from "./language-bar";
import { editingSentence } from "./presence";

/** An active agent the viewer may see; anonymous visitors see leases only. */
export interface ActiveAgent {
  principal: Principal;
  openChanges: number;
  activeLeases: number;
}

function Label({ children }: { children: string }) {
  return <h2 className="mb-2 text-sm font-medium text-ink-3">{children}</h2>;
}

export function RepoNote({
  repo,
  languages,
  leases,
  leasesVisible,
  agents,
  signedIn,
}: {
  repo: Repo;
  languages: LanguageStat[];
  leases: Lease[];
  leasesVisible: boolean;
  agents: ActiveAgent[];
  signedIn: boolean;
}) {
  const stars = Number(repo.stars);
  const forks = Number(repo.forks);
  const leaseHolders = new Set(leases.map((l) => l.holder?.id));
  const idleAgents = agents.filter((a) => !leaseHolders.has(a.principal.id));
  const busy = leases.length > 0 || idleAgents.length > 0;
  return (
    <aside aria-label="About this repository" className="flex min-w-0 flex-col gap-7 text-base text-ink-2">
      {repo.topics.length ? (
        <section>
          <Label>Topics</Label>
          <ul className="flex flex-wrap gap-x-4 gap-y-1">
            {repo.topics.map((t) => (
              <li key={t}>
                <Link href={"/topics/" + encodeURIComponent(t)} className="inline-flex min-h-6 items-center text-accent-ink hover:underline">
                  {t}
                </Link>
              </li>
            ))}
          </ul>
        </section>
      ) : null}
      {languages.length ? (
        <section>
          <Label>Languages</Label>
          <LanguageBar languages={languages} />
        </section>
      ) : null}
      <section>
        <Label>Details</Label>
        <dl className="flex flex-col gap-1.5">
          <div className="flex gap-3">
            <dt className="w-16 shrink-0 text-ink-3">License</dt>
            <dd>{repo.licenseSpdx || "None detected"}</dd>
          </div>
          <div className="flex gap-3">
            <dt className="w-16 shrink-0 text-ink-3">Stars</dt>
            <dd className="num">{compact(stars)}</dd>
          </div>
          <div className="flex gap-3">
            <dt className="w-16 shrink-0 text-ink-3">Forks</dt>
            <dd className="num">{compact(forks)}</dd>
          </div>
          {repo.homepage ? (
            <div className="flex gap-3">
              <dt className="sr-only">Homepage</dt>
              <dd className="min-w-0">
                <a href={repo.homepage} rel="noreferrer noopener" target="_blank" className="inline-flex max-w-full items-center gap-1.5 text-accent-ink hover:underline">
                  <Icon name="external" size={14} />
                  <span className="truncate">{repo.homepage.replace(/^https?:\/\//, "")}</span>
                </a>
              </dd>
            </div>
          ) : null}
        </dl>
      </section>
      <section>
        <Label>Editing now</Label>
        {busy ? (
          <ul className="flex flex-col gap-3">
            {leases.map((l) => (
              <li key={l.id} className="flex items-start gap-2.5">
                <Avatar principal={toAvatar(l.holder)} size={28} />
                <span className="min-w-0 pt-0.5 [overflow-wrap:anywhere]">{editingSentence([l])}</span>
              </li>
            ))}
            {idleAgents.map((a) => (
              <li key={a.principal.id} className="flex items-start gap-2.5">
                <Avatar principal={toAvatar(a.principal)} size={28} />
                <span className="min-w-0 pt-0.5">
                  {a.principal.display} is active, with {plural(a.openChanges, "open change request")}.
                </span>
              </li>
            ))}
          </ul>
        ) : (
          <p className="text-ink-3">{leasesVisible || signedIn ? "Nobody is editing right now." : "Sign in to see who is editing here."}</p>
        )}
      </section>
      <section>
        <Label>More</Label>
        <ul className="flex flex-col gap-1">
          <li>
            <Link href={repoHref(repo.fullName, "code?view=history")} className="inline-flex min-h-6 items-center text-accent-ink hover:underline">
              Recent history
            </Link>
          </li>
          <li>
            <Link href={repoHref(repo.fullName, "activity")} className="inline-flex min-h-6 items-center text-accent-ink hover:underline">
              Activity
            </Link>
          </li>
        </ul>
      </section>
      <section>
        <Label>Download</Label>
        <ArchiveDownload fullName={repo.fullName} />
      </section>
    </aside>
  );
}
