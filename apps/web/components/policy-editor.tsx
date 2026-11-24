"use client";

import Link from "next/link";
import { useRef, useState } from "react";
import { browserClient } from "@/lib/api/browser";
import type { PolicyEvaluation } from "@/lib/api/types";
import { CopyAs } from "./copy-as";
import { DiffClassBadge } from "./domain";
import { Icon } from "./icons";
import { useToast } from "./toast";
import { Badge, Button, cx } from "./ui";
import { explainRequirement } from "./change-actions";
import { repoHref } from "@/lib/routes";

/** Edit .bee/policy.toml and replay recent changes against it before anything lands. */
/**
 * `unavailable` carries the server's answer when it does not serve PolicyService: the file is then
 * shown as read from the repo, and Evaluate stays disabled with the reason beside it.
 */
export function PolicyEditor({ repo, source, epoch, path, unavailable }: { repo: string; source: string; epoch: string; path: string; unavailable?: { reason: string; message: string } }) {
  const [text, setText] = useState(source);
  const [result, setResult] = useState<PolicyEvaluation | null>(null);
  const [busy, setBusy] = useState(false);
  const gutter = useRef<HTMLDivElement>(null);
  const toast = useToast();
  const lines = text.split("\n").length;
  const errorLines = new Set(result?.errors.map((e) => e.line) ?? []);
  const req = { repo: { fullName: repo }, source: text, sample: 20 };
  const dirty = text !== source;

  async function evaluate() {
    if (unavailable) return;
    setBusy(true);
    try {
      setResult(await browserClient().policy.evaluatePolicy(req));
    } catch (e) {
      toast.error(e, "Evaluation failed");
    } finally {
      setBusy(false);
    }
  }

  const blocked = result?.decisions.filter((d) => d.current && !d.proposed) ?? [];
  const unblocked = result?.decisions.filter((d) => !d.current && d.proposed) ?? [];

  return (
    <div className="grid grid-cols-1 gap-5 xl:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
      <section aria-labelledby="policy-src" className="min-w-0">
        <header className="mb-3 flex min-h-9 flex-wrap items-center gap-x-3 gap-y-2">
          <h2 id="policy-src" className="mono text-md font-semibold">
            {path}
          </h2>
          {epoch ? <span className="mono text-sm text-ink-3">version {epoch}</span> : null}
          {dirty ? <Badge tone="warn">edited</Badge> : null}
          <div className="ml-auto flex items-center gap-1">
            {dirty ? (
              <Button size="sm" variant="ghost" onClick={() => { setText(source); setResult(null); }}>
                Reset
              </Button>
            ) : null}
            <CopyAs compact op={{ service: "PolicyService", method: "EvaluatePolicy", request: req, label: "Evaluate this policy" }} />
            <Button size="sm" variant="primary" icon="play" onClick={evaluate} disabled={busy || !!unavailable} aria-describedby={unavailable ? "policy-unavailable" : undefined}>
              {busy ? "Evaluating…" : "Evaluate"}
            </Button>
          </div>
        </header>
        <div className="flex max-h-[70vh] overflow-hidden rounded-lg border border-line">
          <div ref={gutter} aria-hidden="true" className="mono overflow-hidden bg-sunken py-2 text-right text-sm leading-5 text-ink-3 select-none">
            {Array.from({ length: lines }, (_, i) => (
              <div key={i} className={cx("px-2", errorLines.has(i + 1) && "bg-bad-soft text-bad")}>
                {i + 1}
              </div>
            ))}
          </div>
          <label htmlFor="policy-text" className="sr-only">
            Policy source
          </label>
          <textarea
            id="policy-text"
            value={text}
            spellCheck={false}
            onChange={(e) => setText(e.target.value)}
            onScroll={(e) => {
              if (gutter.current) gutter.current.scrollTop = e.currentTarget.scrollTop;
            }}
            onKeyDown={(e) => {
              if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
                e.preventDefault();
                void evaluate();
              }
            }}
            className="mono min-h-[60vh] flex-1 resize-none overflow-auto bg-surface px-3 py-2 text-sm leading-5 whitespace-pre text-ink outline-none"
          />
        </div>
        <p className="mt-3 text-sm text-ink-3">⌘ Enter evaluates. A policy takes effect only once it lands on the default branch, under the current policy.</p>
      </section>

      <section aria-labelledby="eval-title" aria-live="polite" className="min-w-0">
        <header className="mb-3 flex min-h-9 items-center">
          <h2 id="eval-title" className="text-xl font-semibold tracking-tight">
            Try it first
          </h2>
        </header>
        {unavailable ? (
          <div id="policy-unavailable" data-testid="unavailable" role="status" className="flex flex-col gap-2 text-base text-ink-2">
            <p className="font-medium text-ink">Validation and replay are not available on this server.</p>
            <p>{unavailable.message}</p>
            <p className="mono text-sm text-ink-3">reason: {unavailable.reason}</p>
            <p>The file on the left is read from the repository as it is on the default ref. Policy still governs every land; propose edits as a change request.</p>
          </div>
        ) : !result ? (
          <div className="flex flex-col gap-2 text-base text-ink-2">
            <p>Edit the rules, then evaluate. Beehive replays the 20 most recent changes against your draft and shows which it would have blocked or let through. Nothing is enforced.</p>
          </div>
        ) : !result.valid ? (
          <div className="flex flex-col gap-2" data-testid="policy-errors">
            <Badge tone="bad">invalid</Badge>
            <ul className="flex flex-col gap-1.5">
              {result.errors.map((e, i) => (
                <li key={i} className="flex gap-2 text-base">
                  <span className="mono shrink-0 text-bad">line {e.line}</span>
                  <span>{e.message}</span>
                </li>
              ))}
            </ul>
          </div>
        ) : (
          <div className="flex flex-col" data-testid="policy-result">
            <div className="flex flex-wrap items-center gap-2 pb-3">
              <Badge tone="ok">valid</Badge>
              <Badge tone={blocked.length ? "bad" : "neutral"}>{blocked.length} newly blocked</Badge>
              <Badge tone={unblocked.length ? "warn" : "neutral"}>{unblocked.length} newly allowed</Badge>
              <span className="text-sm text-ink-3">of {result.decisions.length} recent changes</span>
            </div>
            <ul className="flex list-disc flex-col gap-0.5 pb-3 pl-5 text-base text-ink-2">
              {result.rules.map((r) => (
                <li key={r}>{r}</li>
              ))}
            </ul>
            <ul className="rule divide-y divide-line">
              {result.decisions.map((d) => {
                const changed = d.current !== d.proposed;
                return (
                  <li key={d.changeRequestId} className={cx("py-3", changed && (d.proposed ? "bg-warn-soft" : "bg-bad-soft"))}>
                    <div className="flex items-center gap-2">
                      <DiffClassBadge cls={d.diffClass} compact />
                      <Link href={repoHref(repo, "changes/" + d.changeRequestId)} className="min-w-0 flex-1 truncate text-base hover:underline">
                        {d.title}
                      </Link>
                      <span className="flex items-center gap-1 text-sm">
                        <span className={d.current ? "text-ok" : "text-ink-3"}>{d.current ? "allowed" : "blocked"}</span>
                        <Icon name="arrowRight" size={12} className="text-ink-3" />
                        <span className={cx("font-medium", d.proposed ? "text-ok" : "text-bad")}>{d.proposed ? "allowed" : "blocked"}</span>
                      </span>
                    </div>
                    {changed && d.missing.length ? <p className="mt-1 text-sm text-ink-2">Would need: {d.missing.map(explainRequirement).join("; ")}</p> : null}
                  </li>
                );
              })}
            </ul>
          </div>
        )}
      </section>
    </div>
  );
}
