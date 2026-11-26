"use client";

import { useState, type ReactNode } from "react";
import { LogViewer } from "./log-viewer";

/** Mount a log stream only while the person has its job open. */
export function JobDisclosure({ summary, annotations, children, defaultOpen, repo, runId, jobId, running }: {
  summary: ReactNode;
  annotations: ReactNode;
  children: ReactNode;
  defaultOpen: boolean;
  repo: string;
  runId: string;
  jobId: string;
  running: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <details open={open} onToggle={(event) => setOpen(event.currentTarget.open)} className="border-b border-line">
      <summary className="cursor-pointer py-4 text-base text-ink hover:text-ink-2">
        {summary}
      </summary>
      <div className="flex min-w-0 flex-col gap-5 pb-5">
        {annotations}
        {open && <LogViewer key={String(running)} repo={repo} runId={runId} jobId={jobId} running={running} />}
        {children}
      </div>
    </details>
  );
}
