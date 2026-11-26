/**
 * A run's "What happened" sentence (docs/spec/web-ui.md §3, Runs):
 *   "Failed after 3m 12s: 1 of 4 jobs failed (test)".
 * Plus a second line naming the first failing step and how many annotations point at code. A
 * running run's elapsed time needs `now`, which the caller passes in; nothing here reads a clock.
 */
import type { Run, RunJob, RunStatus } from "./api/types";
import { listing, shortListing } from "./checkwords";
import { between, duration } from "./format";

export interface RunSummary {
  status: RunStatus;
  tone: "ok" | "bad" | "neutral" | "warn";
  /** "Failed after 3m 12s: 1 of 4 jobs failed (test)". */
  sentence: string;
  /** "The step “cargo test” failed in test. 3 annotations point at code." May be empty. */
  detail: string;
}

function jobs(n: number): string {
  return n + (n === 1 ? " job" : " jobs");
}

function elapsed(run: Run, now?: number): string {
  const end = run.finishedAt ?? (now !== undefined ? new Date(now).toISOString() : undefined);
  const ms = between(run.startedAt, end);
  return Number.isFinite(ms) && ms >= 0 ? duration(ms) : "";
}

function names(js: RunJob[]): string {
  return shortListing(js.map((j) => j.name));
}

export function runSummary(run: Run, now?: number): RunSummary {
  const total = run.jobs.length;
  const by = (s: RunStatus) => run.jobs.filter((j) => j.status === s);
  const failed = by("failed");
  const passed = by("passed");
  const running = by("running");
  const cancelled = by("cancelled");
  const t = elapsed(run, now);
  const after = (word: string) => word + (t ? " after " + t : "");
  const ann = run.annotations.length;
  const annText = ann ? ann + (ann === 1 ? " annotation points" : " annotations point") + " at code." : "";

  switch (run.status) {
    case "passed": {
      const sentence = "Passed" + (t ? " in " + t : "") + ": " + (total === 0 ? "no jobs ran" : total === 1 ? "the one job passed" : "all " + total + " jobs passed");
      return { status: run.status, tone: "ok", sentence, detail: annText };
    }
    case "failed": {
      const head = failed.length ? failed.length + " of " + jobs(total) + " failed (" + names(failed) + ")" : "no job reported a failure";
      const job = failed[0];
      const step = job?.steps.find((s) => s.status === "failed");
      const where = step && job ? "The step “" + step.name + "” failed in " + job.name + "." : "";
      return { status: run.status, tone: "bad", sentence: after("Failed") + ": " + head, detail: [where, annText].filter(Boolean).join(" ") };
    }
    case "cancelled": {
      const done = passed.length + failed.length;
      const head = total ? done + " of " + jobs(total) + " finished before it stopped" : "no jobs ran";
      return { status: run.status, tone: "warn", sentence: after("Cancelled") + ": " + head, detail: cancelled.length ? listing(cancelled.map((j) => j.name)) + (cancelled.length === 1 ? " was" : " were") + " cancelled." : "" };
    }
    case "running": {
      const done = passed.length + failed.length;
      const head = done + " of " + jobs(total) + " done" + (running.length ? ", " + names(running) + (running.length === 1 ? " is" : " are") + " running" : "");
      const warn = failed.length ? names(failed) + " already failed." : "";
      return { status: run.status, tone: failed.length ? "bad" : "neutral", sentence: "Running" + (t ? " for " + t : "") + ": " + head, detail: [warn, annText].filter(Boolean).join(" ") };
    }
    // Keep approval holds distinct from jobs queued for a runner on the CI detail page.
    case "waiting":
      return { status: run.status, tone: "neutral", sentence: "Waiting on a person.", detail: annText };
    case "protection":
      return { status: run.status, tone: "neutral", sentence: "Waiting for deployment protection.", detail: annText };
    case "skipped":
      return { status: run.status, tone: "neutral", sentence: "Skipped.", detail: annText };
    case "neutral":
      return { status: run.status, tone: "neutral", sentence: "Completed with a neutral result.", detail: annText };
    case "stale":
      return { status: run.status, tone: "neutral", sentence: "Marked stale.", detail: annText };
    case "completed":
      return { status: run.status, tone: "neutral", sentence: "Completed without a reported result.", detail: annText };
    default:
      return { status: run.status, tone: "neutral", sentence: "Queued: " + (total ? jobs(total) + " waiting for a runner" : "waiting for a runner"), detail: "" };
  }
}
