"use client";

import { useRouter } from "next/navigation";
import { useEffect, useRef, useState } from "react";
import { browserClient } from "@/lib/api/browser";
import type { LogLine } from "@/lib/api/types";
import { BeeError } from "@/lib/api/transport";
import { CopyAs } from "./copy-as";
import { LiveDot } from "./domain";
import { useToast } from "./toast";
import { Badge, Button, cx } from "./ui";

/** Streams a job's log over WorkflowService.StreamLogs and follows the tail while running. */
export function LogViewer({ repo, runId, jobId, running }: { repo: string; runId: string; jobId: string; running: boolean }) {
  const [lines, setLines] = useState<LogLine[]>([]);
  const [done, setDone] = useState(!running);
  const [error, setError] = useState<string | null>(null);
  const [follow, setFollow] = useState(true);
  const box = useRef<HTMLDivElement>(null);
  const router = useRouter();

  useEffect(() => {
    const ctl = new AbortController();
    (async () => {
      try {
        for await (const l of browserClient().workflow.streamLogs({ repo: { fullName: repo }, runId, jobId }, { signal: ctl.signal })) {
          setLines((xs) => [...xs, l]);
        }
        if (!ctl.signal.aborted) {
          setDone(true);
          if (running) router.refresh();
        }
      } catch (e) {
        if (!ctl.signal.aborted) setError(e instanceof BeeError ? e.reason + ": " + e.message : String(e));
      }
    })();
    return () => ctl.abort();
  }, [repo, runId, jobId, running, router]);

  useEffect(() => {
    if (follow && box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [lines, follow]);

  return (
    <div className="flex flex-col">
      <div className="flex flex-wrap items-center gap-2 border-b border-line px-3.5 py-2">
        <h3 className="text-base font-semibold">Log</h3>
        {error ? <Badge tone="bad">{error}</Badge> : done ? <Badge tone="neutral">{lines.length} lines</Badge> : <LiveDot label="Streaming" />}
        <label className="ml-auto flex items-center gap-1.5 text-sm text-ink-2">
          <input type="checkbox" checked={follow} onChange={(e) => setFollow(e.target.checked)} className="accent-[var(--accent)]" />
          Follow
        </label>
        <CopyAs compact op={{ service: "WorkflowService", method: "StreamLogs", request: { repo: { fullName: repo }, runId, jobId }, label: "Stream this log" }} />
      </div>
      <div
        ref={box}
        className="mono max-h-[60vh] min-h-48 overflow-auto bg-sunken py-2 text-sm leading-5 scrollbar-thin"
        role="log"
        aria-live={done ? "off" : "polite"}
        aria-label="Job log"
        tabIndex={0}
        data-testid="log"
        onScroll={(e) => {
          const el = e.currentTarget;
          const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
          if (!atBottom && follow) setFollow(false);
        }}
      >
        {lines.map((l) => (
          <div key={l.n} className={cx("flex gap-3 px-3.5", l.level === "group" && "mt-1 font-semibold text-ink", l.level === "error" && "bg-bad-soft text-bad", l.level === "warning" && "text-warn")}>
            <span className="num w-8 shrink-0 text-right text-ink-3 select-none">{l.n}</span>
            <span className="whitespace-pre-wrap [overflow-wrap:anywhere]">
              {l.level === "group" ? "▸ " : ""}
              {l.text}
            </span>
          </div>
        ))}
        {!done && !error ? <div className="px-3.5 pl-[58px] text-ink-3 animate-pulse-dot">…</div> : null}
      </div>
    </div>
  );
}

export function RerunButtons({ repo, runId, hasFailed, running }: { repo: string; runId: string; hasFailed: boolean; running: boolean }) {
  const router = useRouter();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const rerun = async (failedOnly: boolean) => {
    setBusy(true);
    try {
      const r = await browserClient().workflow.rerun({ repo: { fullName: repo }, id: runId, failedOnly });
      toast.push({ tone: "ok", title: "Attempt " + r.attempt + " started" });
      router.refresh();
    } catch (e) {
      toast.error(e, "Could not rerun");
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="flex items-center gap-1.5">
      {hasFailed ? (
        <Button size="sm" variant="primary" icon="refresh" disabled={busy} onClick={() => rerun(true)}>
          Rerun failed jobs
        </Button>
      ) : null}
      <Button size="sm" icon="refresh" disabled={busy || running} onClick={() => rerun(false)}>
        Rerun all
      </Button>
      <CopyAs compact op={{ service: "WorkflowService", method: "Rerun", request: { repo: { fullName: repo }, id: runId, failedOnly: hasFailed }, label: "Rerun" }} />
    </div>
  );
}
