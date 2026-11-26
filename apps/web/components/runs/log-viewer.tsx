"use client";

import { LoadingBee } from "@/components/bees/pieces";

import { useRouter } from "next/navigation";
import { useEffect, useRef, useState } from "react";
import { browserClient } from "@/lib/api/browser";
import type { LogLine } from "@/lib/api/types";
import { BeeError } from "@/lib/api/transport";
import { Button, cx } from "@/components/kit";
import { State } from "@/components/parts";

/** Streams a job's log over WorkflowService.StreamLogs and follows the tail while running. */
export function LogViewer({ repo, runId, jobId, running }: { repo: string; runId: string; jobId: string; running: boolean }) {
  const [lines, setLines] = useState<LogLine[]>([]);
  const [done, setDone] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [follow, setFollow] = useState(true);
  const box = useRef<HTMLDivElement>(null);
  const router = useRouter();

  useEffect(() => {
    const ctl = new AbortController();
    (async () => {
      try {
        for await (const l of browserClient().workflow.streamLogs({ repo: { fullName: repo }, runId, jobId }, { signal: ctl.signal })) {
          if (ctl.signal.aborted) break;
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
  }, [repo, runId, jobId, router, running]);

  useEffect(() => {
    if (follow && box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [lines, follow]);

  return (
    <div className="flex flex-col">
      <div className="flex flex-wrap items-center gap-2 py-2">
        <h3 className="text-base font-semibold">Log</h3>
        {error ? <span role="alert" className="text-sm text-ink-2">{error}</span> : done ? <span className="num text-sm text-ink-2">{lines.length} lines</span> : <State className="text-sm" shape="running">{running ? "Streaming" : "Loading log…"}</State>}
        <Button size="sm" variant="ghost" className="ml-auto" aria-pressed={follow} onClick={() => setFollow(!follow)}>Follow log</Button>
      </div>
      <div
        ref={box}
        className="mono max-h-[60vh] min-h-48 overflow-auto bg-sunken py-2 text-sm leading-5 scrollbar-thin"
        role="log"
        aria-live={running && !done ? "polite" : "off"}
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
          <div key={l.n} className={cx("flex gap-3 px-3.5", l.level === "group" && "mt-1 font-semibold text-ink", l.level === "error" && "font-medium text-ink")}>
            <span className="num w-8 shrink-0 text-right text-ink-3 select-none">{l.n}</span>
            <span className="whitespace-pre-wrap [overflow-wrap:anywhere]">
              {l.level === "group" ? "▸ " : ""}
              {l.text}
            </span>
          </div>
        ))}
        {!done && !error ? <div className="px-3.5 pl-[58px]"><LoadingBee text="Waiting for logs…" /></div> : null}
      </div>
    </div>
  );
}
