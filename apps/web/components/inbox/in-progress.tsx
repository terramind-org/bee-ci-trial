/** "In progress": one row per working agent, what it is doing in a sentence, where, and a four-step hint. */
import Link from "next/link";
import { Avatar, toAvatar } from "@/components/avatar";
import { pathText } from "@/lib/api/ids";
import { STEPS, type AgentWork, type Progress } from "@/lib/inbox/progress";
import { runtimeName } from "@/lib/inbox/words";
import { repoHref } from "@/lib/routes";

export function InProgress({ work }: { work: AgentWork[] }) {
  return (
    <ul className="flex flex-col" data-testid="in-progress">
      {work.map((w) => {
        const p = w.agent.principal;
        const repo = w.change?.repo ?? w.leases[0]?.repo ?? "";
        const files = [...new Set(w.leases.flatMap((l) => l.lease.paths.map(pathText)))];
        const runtime = runtimeName(p.runtime);
        return (
          <li key={p.id} className="grid grid-cols-[36px_minmax(0,1fr)] items-start gap-x-4 gap-y-3 py-4 sm:grid-cols-[36px_minmax(0,1fr)_168px]">
            <Avatar principal={toAvatar(p)} size={36} className="mt-0.5" />
            <div className="min-w-0">
              <p className="text-lg leading-[26px] [overflow-wrap:anywhere]">
                <span className="font-semibold">{p.display}</span>
                {w.change ? (
                  <>
                    {" "}
                    is working on{" "}
                    <Link href={repoHref(w.change.repo, "changes/" + w.change.change.id)} className="underline decoration-line-strong underline-offset-[5px] hover:decoration-ink">
                      {w.change.change.title}
                    </Link>
                  </>
                ) : (
                  <>: {w.leases[0]?.lease.note || "editing"}</>
                )}
                {repo ? (
                  <>
                    {" "}
                    <span className="text-ink-2">in</span>{" "}
                    <Link href={repoHref(repo)} className="text-ink-2 hover:text-ink hover:underline">
                      {repo}
                    </Link>
                  </>
                ) : null}
              </p>
              {runtime || files.length ? (
                <p className="mt-1 truncate text-base text-ink-3">
                  {runtime ? <span>via {runtime}</span> : null}
                  {files.length ? (
                    <>
                      {runtime ? <span aria-hidden="true"> · </span> : null}
                      <span className="sr-only">editing </span>
                      <span className="mono text-sm text-ink-2">{files.join(", ")}</span>
                    </>
                  ) : null}
                </p>
              ) : null}
            </div>
            {w.progress ? <Steps progress={w.progress} /> : null}
          </li>
        );
      })}
    </ul>
  );
}

/** Four short segments and the current step in words. Nothing pulses; the words carry the state. */
export function Steps({ progress }: { progress: Progress }) {
  const stuck = progress.tone === "stuck";
  return (
    <div className="col-start-2 pt-1 sm:col-start-3" role="img" aria-label={`${progress.label} (step ${progress.step + 1} of ${STEPS.length}: ${STEPS[progress.step]})`}>
      <p className={"text-sm sm:text-right " + (stuck ? "font-medium text-bad" : "text-ink-2")}>{progress.label}</p>
      <div className="mt-2 flex gap-1 sm:justify-end" aria-hidden="true">
        {STEPS.map((s, i) => (
          <span key={s} title={s} className={"h-1 w-8 rounded-full " + (i < progress.step ? "bg-ink-3" : i === progress.step ? (stuck ? "bg-bad" : "bg-ink") : "bg-line-strong")} />
        ))}
      </div>
    </div>
  );
}
