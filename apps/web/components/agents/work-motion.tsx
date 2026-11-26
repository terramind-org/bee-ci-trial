"use client";

import { createContext, useContext, useState, type ReactNode } from "react";
import { Capping, Claiming, PauseSmoke, YourBeeArrives } from "@/components/bees/pieces";
import { useEventTransitions } from "@/components/bees/use-once";
import { LiveRefresh } from "@/components/live-refresh";

const noTransitions = {};
const WorkTransitions = createContext<Readonly<Record<string, string>>>(noTransitions);
const pieces = { cap: Capping, claim: Claiming, pause: PauseSmoke, join: YourBeeArrives };
const workEvents = ["swarm.*", "agent_run.*"];

export function WorkMotion({ repo, scope, snapshot, ids, children }: {
  repo: string;
  scope: string;
  snapshot: Record<string, string>;
  ids?: string[];
  children: ReactNode;
}) {
  // Compare confirmed snapshots above the rows so a row moving into Paused keeps its transition.
  const transitions = useEventTransitions(repo + ":" + scope, snapshot, true);
  return (
    <WorkTransitions.Provider value={transitions}>
      <LiveRefresh repo={repo} kinds={workEvents} ids={ids} />
      {children}
    </WorkTransitions.Provider>
  );
}

export function WorkCue({ piece, watch, text }: { piece: keyof typeof pieces; watch: string; text: string }) {
  const transitions = useContext(WorkTransitions);
  const Picture = pieces[piece];
  const eventKey = transitions[watch];
  return <Picture eventKey={eventKey} text={eventKey ? text : ""} />;
}

export function FinishedWork({ heading, completions, children }: {
  heading: string;
  completions: { watch: string; text: string }[];
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const transitions = useContext(WorkTransitions);
  return (
    <details className="border-t border-line pt-2" onToggle={(event) => setOpen(event.currentTarget.open)}>
      <summary className="link min-h-6 cursor-pointer text-sm text-ink-2">
        <h2 className="inline font-medium">{heading}</h2>
        {/* A live completion can move into this closed section. Show its cap at the count. */}
        {open ? null : completions.filter(({ watch }) => transitions[watch]).map(({ watch, text }) => (
          <span key={watch} className="relative ml-2 inline-block size-4 align-middle">
            <Capping eventKey={transitions[watch]} text={text} />
          </span>
        ))}
      </summary>
      <WorkTransitions.Provider value={open ? transitions : noTransitions}>
        <div className="mt-3">{children}</div>
      </WorkTransitions.Provider>
    </details>
  );
}
