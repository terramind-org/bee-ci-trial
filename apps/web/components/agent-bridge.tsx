"use client";

/**
 * The agent bridge, created once in the browser, and its WebMCP registration.
 *
 * UI SDK's bridge (`createAgent`) lets an agent read the kit's components and call their
 * actions: `snapshot`, `query`, `act`. `registerTools` offers those three to the browser's own
 * agent through WebMCP (`document.modelContext.registerTool`). In a browser without WebMCP
 * nothing is registered and this component does nothing; agents then use MCP or the CLI.
 *
 * The bridge itself is given no `allow`, so anything on the page that calls it directly cannot
 * press a control marked for a person at all ("Needs a person"). The browser's agent goes
 * through `allow` below: a marked step runs only when a person is signed in and agrees in the
 * kit's dialog (lib/agent-allow.ts).
 */
import { createAgent, registerTools } from "@ui-sdk/react/agent";
import { useEffect } from "react";
import { allowStep, hasWebMcp, registerSafely } from "@/lib/agent-allow";
import type { PrincipalKind } from "@/lib/api/types";
import { askPerson } from "./kit/confirm";

type Bridge = ReturnType<typeof createAgent>;
let bridge: Bridge | null = null;

/** The page's one bridge, or null on the server. */
export function agentBridge(): Bridge | null {
  if (typeof window === "undefined") return null;
  return (bridge ??= createAgent());
}

// Created as the script loads, before the first component mounts, so it hears every one.
agentBridge();

export function AgentBridge({ kind }: { kind: PrincipalKind | null }) {
  useEffect(() => {
    const agent = agentBridge();
    if (!agent || !hasWebMcp()) return;
    // Browsers differ in what `registerTool` returns; a failure here must never break the page.
    return registerSafely(() =>
      registerTools(agent, {
        allow: (step, { signal }) =>
          allowStep(step, kind, (sentence) =>
            askPerson({ title: "An agent in your browser wants to do this", description: sentence + ".", confirmLabel: "Allow", cancelLabel: "Refuse" }, { signal }),
          ),
      }),
    );
  }, [kind]);
  return null;
}
