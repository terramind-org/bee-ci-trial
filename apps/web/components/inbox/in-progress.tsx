/** In progress: one quiet row per working agent, what it is doing, where, and how far along. */
import Link from "next/link";
import { pathText } from "@/lib/api/ids";
import { State, type Shape } from "@/components/parts";
import type { AgentWork, Progress } from "@/lib/inbox/progress";
import { repoHref } from "@/lib/routes";

const SHAPE: Record<number, Shape> = { 0: "open", 1: "running", 2: "waiting", 3: "passed" };

/** The step in words with its shape: "Checks running". */
export function Step({ progress }: { progress: Progress }) {
  return <State shape={progress.tone === "stuck" ? "failed" : (SHAPE[progress.step] ?? "open")}>{progress.label}</State>;
}

/** What one agent is doing, as plain text: shared with the agent's view. */
export function workSentence(w: AgentWork): string {
  const name = w.agent.principal.display || w.agent.principal.id;
  const repo = w.change?.repo ?? w.leases[0]?.repo ?? "";
  const what = w.change ? " is working on " + w.change.change.title : ": " + (w.leases[0]?.lease.note || "editing");
  return name + what + (repo ? " in " + repo : "");
}

export function InProgress({ work }: { work: AgentWork[] }) {
  return (
    <ul className="flex flex-col divide-y divide-line" data-testid="in-progress">
      {work.map((w) => {
        const p = w.agent.principal;
        const repo = w.change?.repo ?? w.leases[0]?.repo ?? "";
        const files = [...new Set(w.leases.flatMap((l) => l.lease.paths.map(pathText)))];
        return (
          <li key={p.id} className="grid grid-cols-1 gap-x-5 gap-y-0.5 py-3 text-md sm:grid-cols-[minmax(0,1fr)_auto]">
            <span className="min-w-0 [overflow-wrap:anywhere]">
              {p.display || p.id}
              {w.change ? (
                <>
                  {" is working on "}
                  <Link href={repoHref(w.change.repo, "changes/" + w.change.change.id)} className="link">
                    {w.change.change.title}
                  </Link>
                </>
              ) : (
                <>: {w.leases[0]?.lease.note || "editing"}</>
              )}
            </span>
            {w.progress ? (
              <span className="text-sm text-ink-2 sm:text-right">
                <Step progress={w.progress} />
              </span>
            ) : null}
            {repo || files.length ? (
              <span className="min-w-0 text-xs text-ink-2 [overflow-wrap:anywhere] sm:col-span-2">
                {repo ? (
                  <Link href={repoHref(repo)} className="link">
                    {repo}
                  </Link>
                ) : null}
                {files.length ? (
                  <>
                    {repo ? <span aria-hidden="true"> · </span> : null}
                    <span className="sr-only">editing </span>
                    <span className="mono">{files.join(", ")}</span>
                  </>
                ) : null}
              </span>
            ) : null}
          </li>
        );
      })}
    </ul>
  );
}
