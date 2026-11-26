"use client";

import { useId, useState } from "react";
import {
  Capping, Claiming, Landing, LoadingBee, PauseSmoke,
  QuietRepository, Refusal, YourBeeArrives,
} from "@/components/bees/pieces";
import { Button } from "@/components/kit";

const samples = [
  { id: "loading", title: "Loading", draw: (key?: string) => <LoadingBee eventKey={key} still={!key} /> },
  { id: "capping", title: "Task finished", draw: (key?: string) => <span className="relative inline-block size-5"><Capping eventKey={key} text="Task finished." /></span> },
  { id: "claiming", title: "Task started", draw: (key?: string) => <Claiming eventKey={key} text="Task started." /> },
  { id: "landing", title: "Merge confirmed", draw: (key?: string) => <Landing eventKey={key} text="Merged into main." /> },
  { id: "refusal", title: "Merge refused", draw: (key?: string) => <Refusal eventKey={key} text="Merge refused. One approval is missing." /> },
  { id: "pause", title: "Work paused", draw: (key?: string) => <PauseSmoke eventKey={key} text="Work paused." /> },
  { id: "quiet", title: "No active work", draw: () => <QuietRepository text="No agents are working in this repository." /> },
  { id: "arrival", title: "Join accepted", draw: (key?: string) => <YourBeeArrives eventKey={key} text="Your request to join was accepted." /> },
] as const;

function MotionSample({ sample }: { sample: typeof samples[number] }) {
  const instance = useId();
  const [play, setPlay] = useState(0);
  return (
    <section data-motion-sample={sample.id} className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-3 gap-y-2 border-t border-line py-4 sm:grid-cols-[minmax(9rem,1fr)_minmax(0,1fr)_minmax(0,1fr)_auto] sm:gap-x-5">
      <h2 className="text-base font-medium">{sample.title}</h2>
      <div data-preview className="col-start-1 row-start-2 flex min-h-8 items-center gap-2 sm:col-auto sm:row-auto">
        {sample.draw(play ? `review:${instance}:${play}` : undefined)}
      </div>
      <div data-still className="col-start-2 row-start-2 flex min-h-8 items-center gap-2 sm:col-auto sm:row-auto">
        {sample.draw()}
      </div>
      <Button size="sm" className="justify-self-end" aria-label={`Play ${sample.title.toLowerCase()}`} onClick={() => setPlay((count) => count + 1)}>
        Play
      </Button>
    </section>
  );
}

export function MotionReview() {
  return (
    <div className="max-w-[840px]">
      <h1 className="mb-4 text-2xl">Event motion</h1>
      <p className="mb-8 max-w-lg text-base text-ink-2">
        Play an event to compare its motion with the reduced-motion still.
      </p>
      <div className="hidden grid-cols-[minmax(9rem,1fr)_minmax(0,1fr)_minmax(0,1fr)_auto] gap-5 pb-3 text-xs text-ink-2 sm:grid">
        <span>Event</span><span>Preview</span><span>Still</span><span className="w-14" />
      </div>
      {samples.map((sample) => <MotionSample key={sample.id} sample={sample} />)}
    </div>
  );
}
