/**
 * The rule for one step a browser's agent asks the page to take (WebMCP, through UI SDK's
 * agent bridge; see components/agent-bridge.tsx).
 *
 * A step that carries `confirm` touches a control marked "needs a person" (`ConfirmButton`,
 * `data-agent-confirm`). It runs only when a person is signed in and says yes in the kit's
 * dialog. An agent's own session, or nobody's, cannot answer for a person: the step is refused,
 * and the bridge tells the agent to leave it to them.
 */
import type { PrincipalKind } from "./api/types";

export type StepDecision = "run" | "ask" | "refuse";

export function decideStep(step: { confirm?: string }, kind: PrincipalKind | null | undefined): StepDecision {
  if (!step.confirm) return "run";
  return kind === "PRINCIPAL_KIND_HUMAN" ? "ask" : "refuse";
}

/**
 * The bridge's `allow`: resolves whether the step may run. `ask` puts the sentence to the
 * person and resolves with their answer; it is only called for a signed-in person.
 */
export async function allowStep(step: { confirm?: string }, kind: PrincipalKind | null | undefined, ask: (sentence: string) => Promise<boolean>): Promise<boolean> {
  const decision = decideStep(step, kind);
  if (decision === "run") return true;
  if (decision === "refuse") return false;
  return ask(step.confirm as string);
}

/** Whether this browser offers WebMCP's registration entry point. */
export function hasWebMcp(doc: unknown = typeof document === "undefined" ? undefined : document): boolean {
  const context = (doc as { modelContext?: { registerTool?: unknown } } | undefined)?.modelContext;
  return typeof context?.registerTool === "function";
}

type ToolContext = { registerTool: (...args: unknown[]) => unknown };

/**
 * Runs `register` with the browser's `registerTool` answering with a promise, then puts the
 * browser's own function back.
 *
 * WebMCP is still an origin trial and browsers disagree: some return a promise from
 * `registerTool`, some return nothing, some throw. UI SDK's `registerTools` assumes a promise
 * and would throw on the others, taking the page down with it. A page must never fail because
 * a tool could not be offered, so every shape becomes a promise here, and anything `register`
 * itself throws is swallowed: the agent then uses MCP or the CLI.
 */
export function registerSafely<T>(register: () => T, doc: unknown = typeof document === "undefined" ? undefined : document): T | undefined {
  const context = (doc as { modelContext?: ToolContext } | undefined)?.modelContext;
  if (typeof context?.registerTool !== "function") return undefined;
  const own = context.registerTool;
  context.registerTool = (...args: unknown[]) => {
    try {
      return Promise.resolve(own.apply(context, args));
    } catch (error) {
      return Promise.reject(error);
    }
  };
  try {
    return register();
  } catch {
    return undefined;
  } finally {
    context.registerTool = own;
  }
}
