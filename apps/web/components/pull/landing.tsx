"use client";

import { createContext, useContext, type ReactNode } from "react";
import { Landing } from "@/components/bees/pieces";
import { useEventTransitions } from "@/components/bees/use-once";
import type { ChangeState } from "@/lib/api/types";

const Landings = createContext<Record<string, string>>({});

/** Keep the prior snapshot above the groups, where a live merge cannot remount it. */
export function PullListLandings({ scope, states, children }: { scope: string; states: Record<string, ChangeState>; children: ReactNode }) {
  const transitions = useEventTransitions(scope, states);
  return <Landings.Provider value={transitions}>{children}</Landings.Provider>;
}

/** A merged row can have moved from a different group during the confirmed refresh. */
export function PullListLanding({ id, number, target }: { id: string; number: string; target: string }) {
  const eventKey = useContext(Landings)[id];
  return <Landing eventKey={eventKey} text={mergeText(number, target)} />;
}

export function PullDetailLanding({ repo, id, number, state, target }: { repo: string; id: string; number: string; state: ChangeState; target: string }) {
  // Enqueue is only a request. Both our own merge and a remote merge are shown on confirmed readback.
  const eventKey = useEventTransitions(repo + "/changes/" + id, { [id]: state })[id];
  return state === "CHANGE_STATE_LANDED" ? <Landing eventKey={eventKey} text={mergeText(number, target)} /> : null;
}

function mergeText(number: string, target: string) {
  return "Pull request #" + number + " merged into " + target + ".";
}
