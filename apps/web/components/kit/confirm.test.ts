import { readFileSync, readdirSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { ConfirmButton } from "./confirm";

/**
 * Controls that approve or destroy something must be marked for agents with UI SDK's
 * `data-agent-confirm`, and `ConfirmButton` is the one place that sets it.
 *
 * What this checks, by reading the source of every component (it renders none of them):
 *
 *   1. Every element that carries the marker (`consequence=`) or the destructive look
 *      (`variant="danger"`) also carries `agentConfirm=`, a sentence for the agent.
 *   2. The destructive look is not put on a raw element with `buttonClass("danger")`, which
 *      would skip the wrapper.
 *   3. A file that calls one of the approving or destroying operations listed below marks a
 *      control with `agentConfirm=` (or, in the admin console, uses its `ConfirmButton`).
 *
 * What it cannot check: a destructive control nobody marked and whose operation is not in the
 * list. Add the operation to `GUARDED_CALLS` when you add such a control.
 */
const COMPONENTS = fileURLToPath(new URL("..", import.meta.url));

function sources(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const path = join(dir, e.name);
    if (e.isDirectory()) return sources(path);
    return e.name.endsWith(".tsx") ? [path] : [];
  });
}

const files = sources(COMPONENTS).map((path) => ({ name: relative(COMPONENTS, path), text: readFileSync(path, "utf8") }));

/** JSX opening tags, attributes included: from `<Name` to the `>` that closes the tag. */
function openingTags(text: string): { tag: string; line: number }[] {
  const out: { tag: string; line: number }[] = [];
  for (const m of text.matchAll(/<[A-Z][\w.]*(?=[\s/>])/g)) {
    let depth = 0;
    let quote = "";
    let end = -1;
    for (let i = m.index + m[0].length; i < text.length; i++) {
      const c = text[i] as string;
      if (quote) {
        if (c === quote) quote = "";
      } else if (c === '"' || c === "'" || c === "`") quote = c;
      else if (c === "{") depth++;
      else if (c === "}") depth--;
      else if (c === ">" && depth === 0 && text[i - 1] !== "=") {
        end = i;
        break;
      }
    }
    if (end !== -1) out.push({ tag: text.slice(m.index, end + 1), line: text.slice(0, m.index).split("\n").length });
  }
  return out;
}

/** Operations that approve or destroy, as the components call them. Hand-kept; see the note above. */
const GUARDED_CALLS = [
  ".submitReview(", ".enqueue(", ".closeChange(", ".approvePost(", ".withdrawPost(",
  ".revoke(", ".dismissAlert(", ".createRelease(", ".updateRepo(", ".setMember(",
  ".createOrgInvitation(", ".setAgentBudget(", ".changePlan(", ".cancelRun(", ".cancelAgentRun(", ".decideSwarmJoin(", ".setSwarmState(", ".disableWorkflow(",
  // Fork and sync confirm the destination namespace and branch affected by each write.
  ".forkRepo(", ".syncFork(",
];

describe("controls that need a person", () => {
  it("renders the sentence where an agent's bridge reads it", () => {
    const html = renderToStaticMarkup(h(ConfirmButton, { agentConfirm: "Approves and merges pull request #41", consequence: "approving" }, "Approve and merge"));
    expect(html).toContain('data-agent-confirm="Approves and merges pull request #41"');
    expect(html).toContain('data-consequence="approving"');
    expect(html).toContain('data-slot="button"');
    expect(html).toContain(">Approve and merge<");
  });

  it("gives a destructive control the quiet danger look and an approving one solid ink, unless told otherwise", () => {
    expect(renderToStaticMarkup(h(ConfirmButton, { agentConfirm: "Revokes this token", consequence: "destructive" }, "Revoke"))).toContain("text-bad");
    expect(renderToStaticMarkup(h(ConfirmButton, { agentConfirm: "Publishes release 1.2", consequence: "approving" }, "Publish"))).toContain("bg-accent");
    expect(renderToStaticMarkup(h(ConfirmButton, { agentConfirm: "Closes it", consequence: "destructive", variant: "ghost" }, "Close"))).not.toContain("text-bad");
  });

  it("finds components to read", () => {
    expect(files.length).toBeGreaterThan(50);
    expect(files.some((f) => f.name === "change-actions.tsx")).toBe(true);
  });

  it("marks every destructive or approving element with a sentence", () => {
    const unmarked: string[] = [];
    for (const f of files) {
      for (const { tag, line } of openingTags(f.text)) {
        const flagged = /(?<![-\w])consequence=/.test(tag) || /(?<![-\w])variant="danger"/.test(tag);
        if (flagged && !/(?<![-\w])agentConfirm=/.test(tag)) unmarked.push(f.name + ":" + line);
      }
    }
    expect(unmarked).toEqual([]);
  });

  it("never gives a raw element the destructive look", () => {
    // The kit itself styles the agreeing button of the person's own confirm dialog.
    const raw = files.filter((f) => !f.name.startsWith("kit/") && /buttonClass\(\s*"danger"/.test(f.text)).map((f) => f.name);
    expect(raw).toEqual([]);
  });

  it("marks a control in every file that calls an approving or destroying operation", () => {
    const missing: string[] = [];
    for (const f of files) {
      const calls = GUARDED_CALLS.filter((c) => f.text.includes(c));
      if (calls.length && !/\bagentConfirm=/.test(f.text)) missing.push(f.name + " calls " + calls.join(" "));
      if (f.name.startsWith("admin/") && /\brun\("delete/.test(f.text) && !f.text.includes("<ConfirmButton")) missing.push(f.name + " deletes without the admin ConfirmButton");
    }
    expect(missing).toEqual([]);
  });

  it("would catch an unmarked destructive button", () => {
    const tags = openingTags('<div><Button size="sm" variant="danger" onClick={() => go(a > b)}>Delete</Button></div>');
    expect(tags.map((t) => t.tag)).toContain('<Button size="sm" variant="danger" onClick={() => go(a > b)}>');
    expect(tags.some((t) => /variant="danger"/.test(t.tag) && !/agentConfirm=/.test(t.tag))).toBe(true);
  });
});
