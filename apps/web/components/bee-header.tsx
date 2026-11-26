"use client";

/**
 * The first lines of every page in the agent's view (`data-mode="bee"`): who the reader is, what
 * this page is, and where its tools are. Plain text, so it reads the same in a snapshot.
 *
 * A person previewing the view is told so instead. Per-page renderers (facts and that page's own
 * tools) arrive with each screen; until then the page below is the person's page as mono text.
 */
import { useSyncExternalStore } from "react";
import { hasWebMcp } from "@/lib/agent-allow";

export interface BeeReader {
  /** The agent's name, or null when a person is previewing. */
  agent: string | null;
  /** The id of the person the agent answers to, when the server names one. */
  answersTo?: string;
}

const subscribe = () => () => {};

export function BeeHeader({ reader }: { reader: BeeReader }) {
  // Known only in the browser; the server and the first client render say nothing about it.
  const webmcp = useSyncExternalStore<boolean | null>(
    subscribe,
    () => hasWebMcp(),
    () => null,
  );
  return (
    <header className="mb-8 border-b border-line pb-6" data-bee-header>
      {/* Not a heading: the page below keeps its own h1. */}
      <p className="font-semibold"># Beehive · agent view</p>
      {reader.agent ? (
        <p className="mt-2">
          You are an agent: {reader.agent}
          {reader.answersTo ? ", answering to " + reader.answersTo : ""}. This is the same page a person sees, as text and tools.
        </p>
      ) : (
        <p className="mt-2">You are a person previewing what an agent sees. This is the same page, as text and tools. Switch back with Person, in the navigation.</p>
      )}
      <p className="mt-2 text-ink-2">
        - Read <a href="/llms.txt" className="link">/llms.txt</a> first: it lists every operation and how to call it.
        <br />- Tools on this page (WebMCP): {webmcp === null ? "checking this browser" : webmcp ? "snapshot, query and act are registered with this browser" : "not available in this browser; use MCP or the CLI"}.
        <br />- A control marked &quot;needs a person&quot; sends the request to that person. Do not look for another way.
      </p>
    </header>
  );
}
