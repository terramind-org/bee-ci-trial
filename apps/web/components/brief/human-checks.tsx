/**
 * Checks grouped in words (docs/spec/web-ui.md §3 "Is it safe?"): one summary sentence, then each
 * group (failed, still running, waiting, passed, skipped) as a sentence with its checks under it.
 * Passed and skipped checks fold away; what needs attention stays open.
 */
import Link from "next/link";
import type { CheckRun } from "@/lib/api/types";
import { checksSentence, groupChecks, type CheckGroup } from "@/lib/checkwords";
import { repoHref } from "@/lib/routes";
import { StatusIcon } from "../domain";
import { Icon } from "../icons";

/**
 * Where a check's details live. Absolute URLs are external CI; a root-relative one names a page in
 * this app from before repos had their own URLs (`/runs/…`), so it is rebased under the repo.
 */
export function checkHref(repo: string, url: string): string {
  if (!url) return repoHref(repo, "runs");
  if (/^https:\/\//.test(url)) return url;
  if (url.startsWith("/") && !url.startsWith("//")) return repoHref(repo, url.slice(1));
  return repoHref(repo, "runs");
}

function CheckRow({ c, repo }: { c: CheckRun; repo: string }) {
  const href = checkHref(repo, c.url);
  const external = /^https:\/\//.test(href);
  return (
    <li className="flex min-h-9 items-start gap-2 py-1">
      <span className="mt-0.5">
        <StatusIcon status={c.status} />
      </span>
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="flex flex-wrap items-baseline gap-x-2">
          {external ? (
            <a href={href} target="_blank" rel="noreferrer noopener" className="inline-flex items-center gap-1 text-base text-ink hover:underline">
              {c.name}
              <Icon name="external" size={13} className="text-ink-3" />
              <span className="sr-only">(opens in a new tab)</span>
            </a>
          ) : (
            <Link href={href} className="text-base text-ink hover:underline">
              {c.name}
            </Link>
          )}
          {c.postLand ? <span className="text-xs text-ink-3">after merging</span> : null}
        </span>
        {c.summary ? <span className="text-sm text-ink-2 [overflow-wrap:anywhere]">{c.summary}</span> : null}
      </span>
    </li>
  );
}

function Group({ g, repo }: { g: CheckGroup; repo: string }) {
  const quiet = g.state === "passed" || g.state === "skipped";
  const list = (
    <ul className="ml-1 flex flex-col">
      {g.checks.map((c) => (
        <CheckRow key={c.id || c.name} c={c} repo={repo} />
      ))}
    </ul>
  );
  if (!quiet)
    return (
      <div className="flex flex-col gap-1" data-check-group={g.state}>
        <p className="text-base font-medium text-ink">{g.sentence}</p>
        {list}
      </div>
    );
  return (
    <details className="group" data-check-group={g.state}>
      <summary className="flex min-h-9 cursor-pointer list-none items-center gap-2 text-base text-ink-2 hover:text-ink [&::-webkit-details-marker]:hidden">
        <Icon name="chevronRight" size={16} className="text-ink-3 transition-transform duration-[160ms] group-open:rotate-90" />
        {g.sentence}
      </summary>
      {list}
    </details>
  );
}

export function HumanChecks({ checks, repo, summary = true }: { checks: CheckRun[]; repo: string; /** False where the page already said the summary sentence above. */ summary?: boolean }) {
  const groups = groupChecks(checks);
  return (
    <div className="flex flex-col gap-3" data-human-checks>
      {summary ? <p className="text-lg text-ink">{checksSentence(checks)}</p> : null}
      {groups.map((g) => (
        <Group key={g.state} g={g} repo={repo} />
      ))}
    </div>
  );
}
