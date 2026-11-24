"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { browserClient } from "@/lib/api/browser";
import type { Principal, Project } from "@/lib/api/types";
import { CopyAs } from "./copy-as";
import { useToast } from "./toast";
import { Button, Select, Textarea } from "./ui";

export function AssignIssue({ repo, number, current, people }: { repo: string; number: number; current: string[]; people: Principal[] }) {
  const router = useRouter();
  const toast = useToast();
  const [who, setWho] = useState(current[0] ?? "");
  const [busy, setBusy] = useState(false);
  const req = { repo: { fullName: repo }, number, assigneeIds: who ? [who] : [] };
  const agents = people.filter((p) => p.kind === "PRINCIPAL_KIND_AGENT");
  const humans = people.filter((p) => p.kind === "PRINCIPAL_KIND_HUMAN");
  return (
    <div className="flex flex-col gap-2">
      <label htmlFor={"assign-" + number} className="text-sm font-medium">
        Assignee
      </label>
      <Select id={"assign-" + number} value={who} onChange={(e) => setWho(e.target.value)}>
        <option value="">Nobody</option>
        <optgroup label="Agents (start working when assigned)">
          {agents.map((a) => (
            <option key={a.id} value={a.id}>
              {a.display} · {a.runtime}
            </option>
          ))}
        </optgroup>
        <optgroup label="People">
          {humans.map((h) => (
            <option key={h.id} value={h.id}>
              {h.display}
            </option>
          ))}
        </optgroup>
      </Select>
      <div className="flex items-center gap-1.5">
        <Button
          size="sm"
          variant="primary"
          disabled={busy || (current[0] ?? "") === who}
          onClick={async () => {
            setBusy(true);
            try {
              const i = await browserClient().issue.assignIssue(req);
              const a = i.assignees[0];
              toast.push({ tone: "ok", title: a ? "Assigned to " + a.display : "Unassigned", detail: a?.kind === "PRINCIPAL_KIND_AGENT" ? "The agent picked it up and moved it to In progress." : undefined });
              router.refresh();
            } catch (e) {
              toast.error(e);
            } finally {
              setBusy(false);
            }
          }}
        >
          Assign
        </Button>
        <CopyAs compact op={{ service: "IssueService", method: "AssignIssue", request: req, label: "Assign this issue" }} />
      </div>
    </div>
  );
}

export function IssueStateButton({ repo, number, state }: { repo: string; number: number; state: "open" | "closed" }) {
  const router = useRouter();
  const toast = useToast();
  const next = state === "open" ? "closed" : "open";
  const req = { repo: { fullName: repo }, number, state: next as "open" | "closed" };
  return (
    <span className="inline-flex items-center gap-1">
      <Button
        size="sm"
        onClick={async () => {
          try {
            await browserClient().issue.setIssueState(req);
            router.refresh();
          } catch (e) {
            toast.error(e);
          }
        }}
      >
        {state === "open" ? "Close issue" : "Reopen"}
      </Button>
      <CopyAs compact op={{ service: "IssueService", method: "SetIssueState", request: req, label: state === "open" ? "Close issue" : "Reopen issue" }} />
    </span>
  );
}

export function IssueComment({ repo, number }: { repo: string; number: number }) {
  const router = useRouter();
  const toast = useToast();
  const [body, setBody] = useState("");
  const req = { repo: { fullName: repo }, number, body };
  return (
    <form
      className="flex flex-col gap-2"
      onSubmit={async (e) => {
        e.preventDefault();
        try {
          await browserClient().issue.commentIssue(req);
          setBody("");
          router.refresh();
        } catch (err) {
          toast.error(err);
        }
      }}
    >
      <label htmlFor={"comment-" + number} className="text-sm font-medium">
        Comment
      </label>
      <Textarea id={"comment-" + number} value={body} onChange={(e) => setBody(e.target.value)} placeholder="Markdown. Mention #issues to link them." />
      <div className="flex items-center gap-1.5">
        <Button type="submit" size="sm" disabled={!body.trim()}>
          Comment
        </Button>
        <CopyAs compact op={{ service: "IssueService", method: "CommentIssue", request: req, label: "Comment on this issue" }} />
      </div>
    </form>
  );
}

/** Keyboard-first board: each card has a "Move to" select; drag and drop is an enhancement. */
export function BoardMove({ repo, project, number, column }: { repo: string; project: Project; number: number; column: string }) {
  const router = useRouter();
  const toast = useToast();
  return (
    <>
      <label htmlFor={"move-" + number} className="sr-only">
        Move #{number} to
      </label>
      <select
        id={"move-" + number}
        value={column}
        onChange={async (e) => {
          try {
            await browserClient().project.moveItem({ repo: { fullName: repo }, projectId: project.id, issueNumber: number, toColumnId: e.target.value });
            router.refresh();
          } catch (err) {
            toast.error(err);
          }
        }}
        className="h-8 rounded-md border border-line bg-surface px-1.5 text-sm text-ink-2"
      >
        {project.columns.map((c) => (
          <option key={c.id} value={c.id}>
            {c.name}
          </option>
        ))}
      </select>
    </>
  );
}
