"use client";

import Link from "next/link";
import { type ReactNode, useEffect, useRef, useState } from "react";
import { browserClient } from "@/lib/api/browser";
import { BeeError } from "@/lib/api/transport";
import type { PolicyEvaluation } from "@/lib/api/types";
import { classInfo } from "@/lib/diffclass";
import { repoHref } from "@/lib/routes";
import { missingInWords } from "@/lib/status";
import { CopyAs } from "./copy-as";
import { Button, Field, Textarea, cx } from "./kit";
import { State } from "./parts";
import { policyRules } from "./settings/policy-rules";

export type PolicyUnavailable = { reason: string; message: string };

const SAMPLE = 20;
const DRAFT_DIRECTORY = "policy-draft";

function shellArg(value: string): string {
  return /^[\w@%+=:,./~^-]+$/.test(value) ? value : "'" + value.replace(/'/g, "'\\''") + "'";
}

function Proposal({ text, path, repo, defaultRef }: { text: string; path: string; repo: string; defaultRef: string }) {
  const [copyStatus, setCopyStatus] = useState("");
  const file = shellArg(path);
  const branch = shellArg(defaultRef);
  const checkout = "bee clone " + shellArg(repo) + " " + DRAFT_DIRECTORY + " --ref " + branch;
  const commit = "bee -C " + DRAFT_DIRECTORY + " commit -m 'Update repository rules' -- " + file;
  const propose = "bee -C " + DRAFT_DIRECTORY + " cr open --title 'Update repository rules' --target " + branch;

  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
      setCopyStatus("Draft copied.");
    } catch {
      setCopyStatus("Select the draft text to copy it.");
    }
  }

  const snippet = "mono max-h-60 overflow-auto rounded-xl bg-sunken p-3 text-sm whitespace-pre-wrap [overflow-wrap:anywhere] text-ink scrollbar-thin";
  return (
    <section id="policy-proposal" aria-labelledby="policy-proposal-heading" className="flex min-w-0 flex-col gap-3 border-t border-line pt-5">
      <h4 id="policy-proposal-heading" className="text-lg font-light">Open a pull request</h4>
      <p className="text-base text-ink-2">Create a fresh working copy in an unused <code>{DRAFT_DIRECTORY}</code> directory.</p>
      <pre className={snippet}><code>{checkout}</code></pre>
      <p className="text-base text-ink-2">Copy the draft into <code>{path}</code> in that working copy. Create its parent directory if needed.</p>
      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" icon="copy" onClick={copy}>Copy draft</Button>
        <span className="text-sm text-ink-2" role="status">{copyStatus}</span>
      </div>
      <pre className={snippet} aria-label="Draft file content" tabIndex={0}><code>{text}</code></pre>
      <p className="text-base text-ink-2">Commit the file and open the pull request.</p>
      <pre className={snippet}><code>{commit + "\n" + propose}</code></pre>
    </section>
  );
}

function Replay({ repo, result }: { repo: string; result: PolicyEvaluation }) {
  if (!result.valid) {
    return (
      <div className="flex flex-col gap-2" data-testid="policy-errors">
        <State shape="failed">Invalid draft</State>
        <ul className="divide-y divide-line">
          {result.errors.map((error, index) => (
            <li key={index} className="flex gap-2 py-2 text-base">
              {error.line > 0 ? <span className="shrink-0 text-ink-2">Line {error.line}</span> : null}
              <span>{error.message}</span>
            </li>
          ))}
        </ul>
      </div>
    );
  }
  const blocked = result.decisions.filter((d) => d.current && !d.proposed).length;
  const allowed = result.decisions.filter((d) => !d.current && d.proposed).length;
  const total = result.decisions.length;
  const rules = policyRules(result.normalizedJson);
  return (
    <div className="flex flex-col gap-3" data-testid="policy-result">
      <State shape="passed">Valid draft</State>
      <p className="text-base text-ink-2">
        {total === 0
          ? "There were no recent pull requests to replay."
          : blocked + " newly blocked and " + allowed + " newly allowed among " + total + " recent pull " + (total === 1 ? "request." : "requests.")}
      </p>
      {rules.length ? (
        <ul className="list-disc space-y-1 pl-5 text-sm text-ink-2" aria-label="Draft rules">
          {rules.map((rule, index) => (
            <li key={index}>{rule.raw ? <code className="mono whitespace-pre-wrap [overflow-wrap:anywhere]">{rule.text}</code> : rule.text}</li>
          ))}
        </ul>
      ) : null}
      {total ? (
        <ul className="divide-y divide-line border-t border-line" aria-label="Replay decisions">
          {result.decisions.map((decision) => (
            <li key={decision.changeRequestId} className="flex flex-col gap-1 py-3">
              <Link href={repoHref(repo, "changes/" + decision.changeRequestId)} className="link self-start text-base">
                {decision.title}
              </Link>
              <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-sm text-ink-2">
                <span>{classInfo(decision.diffClass).label}</span>
                <span className="sr-only">Current rules</span>
                <State shape={decision.current ? "passed" : "waiting"}>{decision.current ? "Allowed" : "Blocked"}</State>
                <span aria-hidden="true">→</span>
                <span className="sr-only">Draft rules</span>
                <State shape={decision.proposed ? "passed" : "waiting"}>{decision.proposed ? "Allowed" : "Blocked"}</State>
              </div>
              {!decision.proposed && decision.missing.length ? (
                <ul className="mt-1 list-disc space-y-1 pl-5 text-sm text-ink-2" aria-label="Requirements under the draft">
                  {decision.missing.map((requirement) => <li key={requirement}>The pull request {missingInWords(requirement)}.</li>)}
                </ul>
              ) : null}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

/** Evaluate a local draft and take its source into a pull request. */
export function PolicyEditor({ repo, source, epoch, path, defaultRef, unavailable, children }: {
  repo: string; source: string; epoch: string; path: string; defaultRef: string; unavailable?: PolicyUnavailable; children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const [propose, setPropose] = useState(false);
  const [text, setText] = useState(source);
  const [evaluation, setEvaluation] = useState<{ text: string; result: PolicyEvaluation } | null>(null);
  const [error, setError] = useState<{ text: string; detail: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const gutter = useRef<HTMLDivElement>(null);
  const editor = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (open) editor.current?.focus();
  }, [open]);
  const req = { repo: { fullName: repo }, source: text, sample: SAMPLE };
  const dirty = text !== source;
  const result = evaluation?.text === text ? evaluation.result : null;
  const errorLines = new Set(result?.errors.map((e) => e.line));

  async function evaluate() {
    if (unavailable || busy) return;
    setBusy(true);
    setError(null);
    setEvaluation(null);
    try {
      setEvaluation({ text, result: await browserClient().policy.evaluatePolicy(req) });
    } catch (e) {
      setError({ text, detail: e instanceof BeeError ? e.reason + " · " + e.message : "Evaluation failed. Try again." });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex min-w-0 flex-col gap-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 className="text-xl font-light">Rules</h2>
        <Button size="sm" aria-expanded={open} aria-controls={open ? "policy-draft" : undefined} onClick={() => setOpen(!open)}>
          {open ? "Hide draft" : "Draft a change"}
        </Button>
      </div>
      {children}
      {open ? (
        <section id="policy-draft" aria-labelledby="policy-draft-heading" className="grid min-w-0 gap-6 border-t border-line pt-5 xl:grid-cols-2">
          <div className="flex min-w-0 flex-col gap-3">
            <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
              <h3 id="policy-draft-heading" className="text-xl font-light">Draft</h3>
              <code className="mono text-sm text-ink-2 [overflow-wrap:anywhere]">{path}</code>
              {epoch ? <span className="text-xs text-ink-3">Version <code className="mono [overflow-wrap:anywhere]">{epoch}</code></span> : null}
            </div>
            <Field label="Policy source" htmlFor="policy-text">
              <div className="flex min-w-0">
                <div ref={gutter} aria-hidden="true" className="mono h-80 shrink-0 overflow-hidden bg-sunken py-2 text-right text-sm leading-5 text-ink-3 select-none">
                  {text.split("\n").map((_, index) => (
                    <div key={index} className={cx("px-2", errorLines.has(index + 1) && "font-medium text-ink")}>{index + 1}</div>
                  ))}
                </div>
                <Textarea
                  id="policy-text"
                  ref={editor}
                  value={text}
                  spellCheck={false}
                  wrap="off"
                  onChange={(event) => setText(event.target.value)}
                  onScroll={(event) => { if (gutter.current) gutter.current.scrollTop = event.currentTarget.scrollTop; }}
                  onKeyDown={(event) => {
                    if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
                      event.preventDefault();
                      void evaluate();
                    }
                  }}
                  className="mono h-80 min-w-0 resize-none overflow-auto rounded-none px-3 py-2 text-sm leading-5 whitespace-pre scrollbar-thin"
                />
              </div>
            </Field>
            <div className="flex flex-wrap items-center gap-2">
              <Button size="sm" variant="primary" onClick={evaluate} disabled={busy || Boolean(unavailable)} aria-describedby={unavailable ? "policy-unavailable" : undefined}>
                {busy ? "Evaluating…" : "Evaluate"}
              </Button>
              {unavailable ? null : <CopyAs compact op={{ service: "PolicyService", method: "EvaluatePolicy", request: req, label: "Evaluate this draft" }} />}
              {dirty ? (
                <Button size="sm" variant="ghost" onClick={() => { setText(source); setEvaluation(null); setError(null); }} disabled={busy}>Reset draft</Button>
              ) : null}
            </div>
            <Button size="sm" className="self-start" aria-expanded={propose} aria-controls={propose ? "policy-proposal" : undefined} onClick={() => setPropose(!propose)}>
              {propose ? "Hide proposal" : "Propose this change"}
            </Button>
            {propose ? <Proposal key={text} text={text} path={path} repo={repo} defaultRef={defaultRef} /> : null}
          </div>
          <section aria-labelledby="policy-replay-heading" aria-live="polite" className="min-w-0">
            <h3 id="policy-replay-heading" className="mb-3 text-xl font-light">Replay</h3>
            {unavailable ? (
              <div id="policy-unavailable" data-testid="unavailable" role="status" className="space-y-2 text-base text-ink-2">
                <p>Evaluation and replay are unavailable on this server.</p>
                <p>{unavailable.message}</p>
                <code className="mono text-sm [overflow-wrap:anywhere]">{unavailable.reason}</code>
              </div>
            ) : error?.text === text ? (
              <p role="alert" className="text-base text-ink-2">{error.detail}</p>
            ) : result ? (
              <Replay repo={repo} result={result} />
            ) : (
              <p className="text-base text-ink-2">
                Evaluate to see what the draft would have allowed or blocked for up to {SAMPLE} recent pull requests.
              </p>
            )}
          </section>
        </section>
      ) : null}
    </div>
  );
}
