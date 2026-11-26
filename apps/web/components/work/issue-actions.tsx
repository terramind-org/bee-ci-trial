"use client";

import { useRouter } from "next/navigation";
import { useEffect, useRef, useState } from "react";
import { browserClient } from "@/lib/api/browser";
import type { Principal, Project } from "@/lib/api/types";
import { expandIssueDraft, ISSUE_TITLE_LIMIT, issueLabels, needsIssueDetail, type IssueDraft } from "@/lib/issues";
import { repoHref } from "@/lib/routes";
import { useToast } from "@/components/toast";
import { askPerson, Button, ConfirmButton, Field, Input, Select, Textarea } from "@/components/kit";

const DESCRIBE = "Describe what you want done. An agent or a person will pick it up.";

function AssigneeOptions({ people }: { people: Principal[] }) {
  const agents = people.filter((p) => p.kind === "PRINCIPAL_KIND_AGENT");
  const humans = people.filter((p) => p.kind === "PRINCIPAL_KIND_HUMAN");
  return (
    <>
      <option value="">Nobody</option>
      {agents.length ? (
        <optgroup label="Agents">
          {agents.map((a) => (
            <option key={a.id} value={a.id}>{(a.display || a.id) + (a.runtime ? " · " + a.runtime : "")}</option>
          ))}
        </optgroup>
      ) : null}
      {humans.length ? (
        <optgroup label="People">
          {humans.map((h) => <option key={h.id} value={h.id}>{h.display || h.id}</option>)}
        </optgroup>
      ) : null}
    </>
  );
}

export function NewIssue({ repo, people }: { repo: string; people: Principal[] }) {
  const router = useRouter();
  const toast = useToast();
  const [draft, setDraft] = useState("");
  const [detail, setDetail] = useState<IssueDraft | null>(null);
  const [busy, setBusy] = useState(false);
  const [labels, setLabels] = useState("");
  const [assignee, setAssignee] = useState("");
  const expanded = !!detail;
  const description = useRef<HTMLTextAreaElement>(null);
  const titleInput = useRef<HTMLInputElement>(null);
  const detailFocus = useRef<"title" | "description">("title");
  useEffect(() => {
    if (expanded && detailFocus.current === "description" && description.current) {
      description.current.focus();
      const end = description.current.value.length;
      description.current.setSelectionRange(end, end);
    } else if (expanded) {
      titleInput.current?.focus();
    }
  }, [expanded]);
  const title = (detail?.title ?? draft).trim();

  function describe(text: string) {
    setDraft(text);
    if (needsIssueDetail(text)) {
      detailFocus.current = "description";
      setDetail(expandIssueDraft(text));
    }
  }

  return (
    <form
      className="mb-8 flex flex-col gap-3"
      data-testid="new-issue"
      onSubmit={async (e) => {
        e.preventDefault();
        if (busy || !title) return;
        setBusy(true);
        try {
          const issue = await browserClient().issue.createIssue({
            repo: { fullName: repo },
            title,
            body: detail?.body.trim() ? detail.body : undefined,
            labels: detail ? issueLabels(labels) : undefined,
            assignees: assignee ? [assignee] : undefined,
          });
          router.push(repoHref(repo, "work/" + issue.number));
        } catch (err) {
          toast.error(err, "Issue not opened");
          setBusy(false);
        }
      }}
    >
      {detail ? (
        <div className="flex flex-col gap-4">
          <Field htmlFor="new-issue-title" label="Title">
            <Input ref={titleInput} id="new-issue-title" value={detail.title} maxLength={ISSUE_TITLE_LIMIT} required disabled={busy} onChange={(e) => setDetail({ ...detail, title: e.target.value })} />
          </Field>
          <Field htmlFor="new-issue-body" label="Description (Markdown)">
            <Textarea ref={description} id="new-issue-body" value={detail.body} rows={5} disabled={busy} onChange={(e) => setDetail({ ...detail, body: e.target.value })} />
          </Field>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field htmlFor="new-issue-labels" label="Labels (comma separated)">
              <Input id="new-issue-labels" value={labels} disabled={busy} onChange={(e) => setLabels(e.target.value)} />
            </Field>
            <Field htmlFor="new-issue-assignee" label="Assignee">
              <Select id="new-issue-assignee" value={assignee} disabled={busy} onChange={(e) => setAssignee(e.target.value)}>
                <AssigneeOptions people={people} />
              </Select>
            </Field>
          </div>
        </div>
      ) : null}
      <div className="flex flex-col gap-2.5 sm:flex-row sm:items-end">
        {!expanded ? (
          <div className="min-w-0 sm:flex-1">
            <label htmlFor="new-issue-quick" className="mb-1.5 block text-base text-ink-2">
              {DESCRIBE}
            </label>
            <Input
              id="new-issue-quick"
              value={draft}
              placeholder="Issue title"
              disabled={busy}
              onChange={(e) => describe(e.target.value)}
              onPaste={(e) => {
                // A single-line input strips newlines on paste. Keep the pasted Markdown intact.
                const pasted = e.clipboardData.getData("text");
                if (!/[\r\n]/.test(pasted)) return;
                e.preventDefault();
                const { selectionStart, selectionEnd } = e.currentTarget;
                describe(draft.slice(0, selectionStart ?? draft.length) + pasted + draft.slice(selectionEnd ?? draft.length));
              }}
            />
          </div>
        ) : null}
        <div className="flex flex-wrap items-center gap-2.5">
          {!expanded ? <Button type="button" onClick={() => setDetail(expandIssueDraft(draft))}>More detail</Button> : null}
          <Button type="submit" variant="primary" disabled={busy || !title}>
            {busy ? "Opening…" : "Open issue"}
          </Button>
        </div>
      </div>
    </form>
  );
}

export function AssignIssue({ repo, number, current, people }: { repo: string; number: number; current: string[]; people: Principal[] }) {
  const router = useRouter();
  const toast = useToast();
  const [who, setWho] = useState(current[0] ?? "");
  const [busy, setBusy] = useState(false);
  const req = { repo: { fullName: repo }, number, assigneeIds: who ? [who] : [] };
  return (
    <div className="flex flex-col gap-2">
      <Field htmlFor={"assign-" + number} label="Assignee">
        <Select id={"assign-" + number} value={who} disabled={busy} onChange={(e) => setWho(e.target.value)}>
          {who && !people.some((p) => p.id === who) ? <option value={who}>{who}</option> : null}
          <AssigneeOptions people={people} />
        </Select>
      </Field>
      <div className="flex items-center gap-1.5">
        <Button
          size="sm"
          disabled={busy || (current[0] ?? "") === who}
          onClick={async () => {
            setBusy(true);
            try {
              const i = await browserClient().issue.assignIssue(req);
              const a = i.assignees[0];
              toast.push({ tone: "ok", title: a ? "Assigned to " + (a.display || a.id) : "Unassigned" });
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
      </div>
    </div>
  );
}

export function IssueStateButton({ repo, number, state }: { repo: string; number: number; state: "open" | "closed" }) {
  const router = useRouter();
  const toast = useToast();
  const closing = state === "open";
  const action = closing ? "Close issue" : "Reopen";
  return (
    <ConfirmButton
      size="sm"
      variant="secondary"
      consequence={closing ? "destructive" : "approving"}
      agentConfirm={(closing ? "Closes issue #" : "Reopens issue #") + number}
      onClick={async () => {
        if (!(await askPerson({ title: (closing ? "Close issue #" : "Reopen issue #") + number + "?", confirmLabel: action, destructive: closing }))) return;
        try {
          await browserClient().issue.setIssueState({ repo: { fullName: repo }, number, state: closing ? "closed" : "open" });
          router.refresh();
        } catch (err) {
          toast.error(err);
        }
      }}
    >
      {action}
    </ConfirmButton>
  );
}

export function IssueComment({ repo, number }: { repo: string; number: number }) {
  const router = useRouter();
  const toast = useToast();
  const [body, setBody] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <form
      className="flex flex-col gap-2"
      onSubmit={async (e) => {
        e.preventDefault();
        if (busy || !body.trim()) return;
        setBusy(true);
        try {
          await browserClient().issue.commentIssue({ repo: { fullName: repo }, number, body });
          setBody("");
          router.refresh();
        } catch (err) {
          toast.error(err);
        } finally {
          setBusy(false);
        }
      }}
    >
      <label htmlFor={"comment-" + number} className="text-xs font-medium text-ink-2">
        Comment
      </label>
      <Textarea id={"comment-" + number} value={body} onChange={(e) => setBody(e.target.value)} disabled={busy} placeholder="Write a comment in Markdown" />
      <div className="flex items-center gap-1.5">
        <Button type="submit" size="sm" variant="primary" disabled={busy || !body.trim()}>
          Comment
        </Button>
      </div>
    </form>
  );
}

/** A native select keeps board moves available to the keyboard and screen readers. */
export function BoardMove({ repo, project, number, column }: { repo: string; project: Project; number: number; column: string }) {
  const router = useRouter();
  const toast = useToast();
  const [pendingColumn, setPendingColumn] = useState<string>();
  return (
    <Select
      aria-label={"Move #" + number + " to"}
      value={pendingColumn ?? column}
      aria-busy={!!pendingColumn}
      disabled={!!pendingColumn}
      onChange={async (e) => {
        setPendingColumn(e.target.value);
        try {
          await browserClient().project.moveItem({ repo: { fullName: repo }, projectId: project.id, issueNumber: number, toColumnId: e.target.value });
        } catch (err) {
          toast.error(err);
        } finally {
          setPendingColumn(undefined);
          router.refresh();
        }
      }}
      className="h-9 w-auto max-w-full pl-3 text-xs"
    >
      {project.columns.map((c) => <option key={c.id} value={c.id}>{c.name}</option>)}
    </Select>
  );
}
