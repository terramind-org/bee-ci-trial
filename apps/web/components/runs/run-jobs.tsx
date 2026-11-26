import Link from "next/link";
import { Quiet, Sep, State } from "@/components/parts";
import type { Annotation, Run, RunJob } from "@/lib/api/types";
import { duration } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { JobDisclosure } from "./job-disclosure";
import { runDuration, RunState } from "./run-facts";

export function JobLine({ job }: { job: RunJob }) {
  const elapsed = runDuration(job);
  return <><span className="font-medium">{job.name}</span> <Sep /> <RunState status={job.status} />{elapsed && <> <Sep /> <span className="num">{elapsed}</span></>}</>;
}

export function AnnotationLine({ annotation, repo }: { annotation: Annotation; repo: string }) {
  const { path, line, title, message, level } = annotation;
  return (
    <>
      <State shape={level === "failure" ? "failed" : level === "warning" ? "waiting" : "open"}>{level === "failure" ? "Failure" : level === "warning" ? "Warning" : "Notice"}</State>
      {title && <> <Sep /> {title}</>}
      {path && <> <Sep /> <Link className="link mono inline-flex min-h-6 items-center [overflow-wrap:anywhere]" href={repoHref(repo, "code?path=" + encodeURIComponent(path) + (line ? "#L" + line : ""))}>{path}{line ? ":" + line : ""}</Link></>}
      {message && <span className="block whitespace-pre-wrap text-ink-2 [overflow-wrap:anywhere]">{message}</span>}
    </>
  );
}

export function StepLine({ step }: { step: RunJob["steps"][number] }) {
  const elapsed = Number(step.durationMs);
  return <>{step.name} <Sep /> <RunState status={step.status} />{elapsed > 0 && <> <Sep /> <span className="num">{duration(elapsed)}</span></>}</>;
}

function Annotations({ annotations, repo, id }: { annotations: Annotation[]; repo: string; id: string }) {
  if (!annotations.length) return null;
  return (
    <section aria-labelledby={id}>
      <h3 id={id} className="mb-1 text-sm font-medium text-ink-2">Annotations</h3>
      <ul className="divide-y divide-line">
        {annotations.map((annotation, i) => <li key={i} className="py-2 text-sm"><AnnotationLine annotation={annotation} repo={repo} /></li>)}
      </ul>
    </section>
  );
}

export function RunJobs({ run, repo, selected }: { run: Run; repo: string; selected: string }) {
  const orphaned = run.annotations.filter((a) => !a.jobId || !run.jobs.some((j) => j.id === a.jobId));
  return (
    <section aria-labelledby="ci-jobs">
      <h2 id="ci-jobs" className="mb-2 text-lg font-light">Jobs</h2>
      {run.jobs.length ? (
        <ul className="border-t border-line">
          {run.jobs.map((job) => (
            <li key={job.id} id={"job-" + job.id}>
              <JobDisclosure
                key={run.attempt + ":" + selected + ":" + (job.status === "failed")}
                defaultOpen={job.status === "failed" || job.id === selected}
                repo={repo}
                runId={run.id}
                jobId={job.id}
                running={job.status === "running"}
                summary={<JobLine job={job} />}
                annotations={<Annotations annotations={run.annotations.filter((a) => a.jobId === job.id)} repo={repo} id={"annotations-" + job.id} />}
              >
                {job.steps.length > 0 && (
                  <section aria-labelledby={"steps-" + job.id}>
                    <h3 id={"steps-" + job.id} className="mb-1 text-sm font-medium text-ink-2">Steps</h3>
                    {job.runner && <p className="mb-1 text-sm text-ink-2">{job.runner}</p>}
                    <ol className="divide-y divide-line">
                      {job.steps.map((step, i) => <li key={i} className="py-2 text-sm [overflow-wrap:anywhere]"><StepLine step={step} /></li>)}
                    </ol>
                  </section>
                )}
              </JobDisclosure>
            </li>
          ))}
        </ul>
      ) : <Quiet>No jobs have been reported for this run.</Quiet>}
      <Annotations annotations={orphaned} repo={repo} id="run-annotations" />
    </section>
  );
}
