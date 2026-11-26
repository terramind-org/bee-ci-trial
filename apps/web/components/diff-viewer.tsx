"use client";

import { useRouter } from "next/navigation";
import { Fragment, useEffect, useMemo, useState } from "react";
import { browserClient } from "@/lib/api/browser";
import { pathBytes, pathText } from "@/lib/api/ids";
import type { DiffItem, ReviewThread } from "@/lib/api/types";
import { countChanges, hunkLines, toSplitRows, type HunkLine } from "@/lib/diff";
import { highlightLines } from "@/lib/highlight";
import { CopyAs } from "./copy-as";
import { DiffClassBadge, PrincipalMark } from "./domain";
import { Icon } from "./icons";
import { Markdown } from "./markdown";
import { Time } from "./time";
import { useToast } from "./toast";
import { Badge, Button, EmptyState, Textarea, cx } from "./ui";
import { repoHref } from "@/lib/routes";

type Mode = "unified" | "split";
const MODE_KEY = "bee-diff-mode";

interface Props {
  repo: string;
  crId: string;
  items: DiffItem[];
  threads: ReviewThread[];
  revision: number;
  latest: number;
  since: number;
  /** Compare views: no comments, no suggestions. */
  readOnly?: boolean;
}

export function DiffViewer({ repo, crId, items, threads, revision, latest, since, readOnly = false }: Props) {
  const [mode, setMode] = useState<Mode>("unified");
  const [viewed, setViewed] = useState<Record<string, boolean>>({});
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const [selected, setSelected] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const router = useRouter();
  const toast = useToast();
  const viewedKey = "bee-viewed:" + crId + ":" + revision;
  const r = { fullName: repo };

  useEffect(() => {
    // Read persisted preferences after hydration (server HTML always renders the default).
    const id = requestAnimationFrame(() => {
      try {
        const m = localStorage.getItem(MODE_KEY);
        if (m === "split" || m === "unified") setMode(m);
        setViewed(JSON.parse(localStorage.getItem(viewedKey) ?? "{}") as Record<string, boolean>);
      } catch {
        /* storage unavailable */
      }
    });
    return () => cancelAnimationFrame(id);
  }, [viewedKey]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (e.key !== "u" || e.metaKey || e.ctrlKey || (t && /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName)) || document.querySelector("[role=dialog]")) return;
      setMode((m) => {
        const n = m === "unified" ? "split" : "unified";
        try {
          localStorage.setItem(MODE_KEY, n);
        } catch {
          /* ignore */
        }
        return n;
      });
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const toggleViewed = (path: string) => {
    setViewed((v) => {
      const n = { ...v, [path]: !v[path] };
      try {
        localStorage.setItem(viewedKey, JSON.stringify(n));
      } catch {
        /* ignore */
      }
      return n;
    });
    setCollapsed((c) => ({ ...c, [path]: !viewed[path] }));
  };

  const suggestionThreads = threads.filter((t) => !t.resolved && t.comments.some((c) => c.suggestion));
  // The API applies suggestion comments; each selected thread contributes its suggestion.
  const commentIds = suggestionThreads.filter((t) => selected.includes(t.id)).flatMap((t) => t.comments.filter((c) => c.suggestion).slice(0, 1).map((c) => c.id));
  const applyOp = { service: "ChangeService" as const, method: "ApplySuggestions", request: { repo: r, changeRequestId: crId, commentIds }, label: "Apply suggestions as a new revision" };

  async function applySelected() {
    setBusy(true);
    try {
      const cr = await browserClient().change.applySuggestions(applyOp.request);
      toast.push({ tone: "ok", title: "Applied " + selected.length + " suggestion" + (selected.length === 1 ? "" : "s"), detail: "Revision r" + cr.revisions.length + " created; checks are running." });
      setSelected([]);
      router.push(repoHref(repo, "changes/" + crId + "/code"));
      router.refresh();
    } catch (e) {
      toast.error(e, "Could not apply suggestions");
    } finally {
      setBusy(false);
    }
  }

  const totals = items.reduce(
    (acc, it) => {
      const c = countChanges(it.hunks);
      return { added: acc.added + c.added, removed: acc.removed + c.removed };
    },
    { added: 0, removed: 0 },
  );
  const reviewedCount = items.filter((i) => viewed[pathText(i.path)]).length;
  const shownPaths = new Set(items.map((i) => pathText(i.path)));
  const orphanThreads = threads.filter((t) => t.outdated || !shownPaths.has(pathText(t.path)));

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <span className="text-ink-2">
          {items.length} file{items.length === 1 ? "" : "s"} · <span className="text-ok">+{totals.added}</span> <span className="text-bad">−{totals.removed}</span>
          {since ? <span className="ml-2 text-ink-3">changes since r{since}</span> : null}
        </span>
        <span className="text-ink-3">
          {reviewedCount}/{items.length} viewed
        </span>
        <div role="radiogroup" aria-label="Diff layout" className="ml-auto hidden rounded-md border border-line p-0.5 md:flex">
          {(["unified", "split"] as Mode[]).map((m) => (
            <button
              key={m}
              type="button"
              role="radio"
              aria-checked={mode === m}
              onClick={() => {
                setMode(m);
                try {
                  localStorage.setItem(MODE_KEY, m);
                } catch {
                  /* ignore */
                }
              }}
              className={cx("h-6 rounded-sm px-2 text-sm capitalize", mode === m ? "bg-hover font-medium text-ink" : "text-ink-2 hover:text-ink")}
            >
              {m}
            </button>
          ))}
        </div>
      </div>

      {items.length > 1 ? (
        <nav aria-label="Files in this change" className="rounded-lg border border-line bg-surface">
          <ul className="divide-y divide-line">
            {items.map((it) => {
              const p = pathText(it.path);
              const c = countChanges(it.hunks);
              return (
                <li key={p}>
                  <a href={"#file-" + encodeURIComponent(p)} className="flex items-center gap-2 px-3 py-1.5 text-sm hover:bg-hover">
                    <Icon name={viewed[p] ? "check" : "file"} size={14} className={viewed[p] ? "text-ok" : "text-ink-3"} />
                    <span className="mono min-w-0 flex-1 truncate">{p}</span>
                    <DiffClassBadge cls={it.diffClass} compact />
                    <span className="num text-xs text-ok">+{c.added}</span>
                    <span className="num text-xs text-bad">−{c.removed}</span>
                  </a>
                </li>
              );
            })}
          </ul>
        </nav>
      ) : null}

      {items.length === 0 ? (
        <div className="rounded-lg border border-line bg-surface">
          <EmptyState title="No differences" icon="check">
            {since ? "Nothing changed between r" + since + " and r" + revision + " in this change's files." : "This revision is identical to its base."}
          </EmptyState>
        </div>
      ) : null}

      {items.map((it) => {
        const p = pathText(it.path);
        const fileThreads = threads.filter((t) => pathText(t.path) === p && !t.outdated);
        return (
          <FileDiff
            key={p}
            item={it}
            mode={mode}
            threads={fileThreads}
            viewed={!!viewed[p]}
            collapsed={!!collapsed[p]}
            onToggleViewed={() => toggleViewed(p)}
            onToggleCollapsed={() => setCollapsed((c) => ({ ...c, [p]: !c[p] }))}
            selected={selected}
            onSelect={(id, on) => setSelected((s) => (on ? [...s, id] : s.filter((x) => x !== id)))}
            repo={repo}
            crId={crId}
            revision={revision}
            canComment={!readOnly && revision === latest && !since}
          />
        );
      })}

      {orphanThreads.length ? (
        <section aria-labelledby="outdated-title" className="rounded-lg border border-line bg-surface">
          <h3 id="outdated-title" className="border-b border-line px-3.5 py-2 text-base font-semibold">
            Earlier threads <span className="font-normal text-ink-3">anchored to lines that have since changed</span>
          </h3>
          <div className="flex flex-col gap-3 p-3">
            {orphanThreads.map((t) => (
              <Thread key={t.id} t={t} repo={repo} selected={selected} onSelect={(id, on) => setSelected((s) => (on ? [...s, id] : s.filter((x) => x !== id)))} />
            ))}
          </div>
        </section>
      ) : null}

      {suggestionThreads.length && !readOnly ? (
        <div className={cx("sticky bottom-3 z-20 flex flex-wrap items-center gap-2 rounded-lg border border-line bg-surface px-3 py-2 shadow-overlay", selected.length ? "" : "opacity-95")}>
          <Icon name="comment" className="text-ink-3" />
          <span className="text-sm text-ink-2">
            {selected.length ? selected.length + " of " + suggestionThreads.length + " suggestions selected" : suggestionThreads.length + " open suggestion" + (suggestionThreads.length === 1 ? "" : "s") + ". Select to apply as one new revision."}
          </span>
          <div className="ml-auto flex items-center gap-1">
            {selected.length === 0 ? (
              <Button size="sm" variant="ghost" onClick={() => setSelected(suggestionThreads.map((t) => t.id))}>
                Select all
              </Button>
            ) : null}
            <CopyAs compact op={applyOp} />
            <Button size="sm" variant="primary" disabled={!selected.length || busy} onClick={applySelected}>
              {busy ? "Applying…" : "Apply " + (selected.length || "") + " suggestion" + (selected.length === 1 ? "" : "s")}
            </Button>
          </div>
        </div>
      ) : null}
    </div>
  );
}

interface FileProps {
  item: DiffItem;
  mode: Mode;
  threads: ReviewThread[];
  viewed: boolean;
  collapsed: boolean;
  onToggleViewed: () => void;
  onToggleCollapsed: () => void;
  selected: string[];
  onSelect: (id: string, on: boolean) => void;
  repo: string;
  crId: string;
  revision: number;
  canComment: boolean;
}

function FileDiff({ item, mode, threads, viewed, collapsed, onToggleViewed, onToggleCollapsed, selected, onSelect, repo, crId, revision, canComment }: FileProps) {
  const path = pathText(item.path);
  const c = countChanges(item.hunks);
  const [composer, setComposer] = useState<number | null>(null);
  const hunks = useMemo(
    () =>
      item.hunks.map((h) => {
        const lines = hunkLines(h);
        const html = highlightLines(lines.map((l) => l.text).join("\n"), path);
        return { h, lines: lines.map((l, k) => ({ ...l, html: html[k] ?? "" })) };
      }),
    [item.hunks, path],
  );
  const placed = new Set<string>();
  const threadsAt = (n: number | null) => {
    if (n === null) return [];
    const out = threads.filter((t) => t.endLine === n && !placed.has(t.id));
    for (const t of out) placed.add(t.id);
    return out;
  };
  const added = !item.old;
  const deleted = !item.new;
  const lineText = (n: number) => hunks.flatMap((x) => x.lines).find((l) => l.newNo === n)?.text ?? "";

  const extras = (n: number | null, cols: number) => {
    if (n === null) return null; // deleted lines carry no new-side anchor
    const ts = threadsAt(n);
    if (!ts.length && composer !== n) return null;
    return (
      <tr>
        <td colSpan={cols} className="border-y border-line bg-sunken px-3 py-2.5">
          <div className="flex max-w-3xl flex-col gap-2.5">
            {ts.map((t) => (
              <Thread key={t.id} t={t} repo={repo} selected={selected} onSelect={onSelect} anchorText={lineText(t.startLine)} />
            ))}
            {composer === n && n !== null ? <Composer repo={repo} crId={crId} revision={revision} path={path} line={n} lineText={lineText(n)} onDone={() => setComposer(null)} /> : null}
          </div>
        </td>
      </tr>
    );
  };

  const numCell = (n: number | null, side: "old" | "new") =>
    side === "new" && n !== null && canComment ? (
      <td className="w-12 border-r border-line pr-2 text-right align-top select-none">
        <button type="button" onClick={() => setComposer(composer === n ? null : n)} className="num block h-6 w-full text-right text-ink-3 hover:text-accent-ink" aria-label={"Comment on line " + n}>
          {n}
        </button>
      </td>
    ) : (
      <td className="num w-12 border-r border-line pr-2 text-right align-top text-ink-3 select-none">{n ?? ""}</td>
    );

  const rowBg = (k: HunkLine["kind"]) => (k === "add" ? "bg-add" : k === "del" ? "bg-del" : "");
  const sign = (k: HunkLine["kind"]) => (k === "add" ? "+" : k === "del" ? "−" : " ");

  return (
    <section id={"file-" + encodeURIComponent(path)} aria-label={"Diff of " + path} className="scroll-mt-16 overflow-hidden rounded-lg border border-line bg-surface">
      <header className="sticky top-12 z-10 flex flex-wrap items-center gap-2 border-b border-line bg-surface px-3 py-2 lg:top-0">
        <button type="button" onClick={onToggleCollapsed} className="text-ink-3 hover:text-ink" aria-expanded={!collapsed} aria-label={(collapsed ? "Expand " : "Collapse ") + path}>
          <Icon name={collapsed ? "chevronRight" : "chevronDown"} />
        </button>
        <span className="mono min-w-0 flex-1 truncate text-sm font-medium text-ink" title={path}>
          {path}
        </span>
        {added ? <Badge tone="ok">added</Badge> : deleted ? <Badge tone="bad">deleted</Badge> : null}
        <DiffClassBadge cls={item.diffClass} />
        <span className="num text-xs text-ok">+{c.added}</span>
        <span className="num text-xs text-bad">−{c.removed}</span>
        <label className="ml-1 flex h-7 items-center gap-1.5 rounded-md border border-line px-2 text-xs text-ink-2 hover:bg-hover">
          <input type="checkbox" checked={viewed} onChange={onToggleViewed} className="accent-[var(--accent)]" />
          Viewed
        </label>
      </header>
      {collapsed ? null : item.binary ? (
        <p className="px-3 py-4 text-sm text-ink-2">Binary file; not shown.</p>
      ) : (
        <div className="overflow-x-auto scrollbar-thin">
          {mode === "unified" ? (
            <table className="mono w-full border-collapse text-sm leading-6">
              <thead className="sr-only">
                <tr>
                  <th scope="col">Old line</th>
                  <th scope="col">New line</th>
                  <th scope="col">Change</th>
                  <th scope="col">Code</th>
                </tr>
              </thead>
              {hunks.map(({ h, lines }, hi) => (
                <tbody key={hi}>
                  <tr>
                    <td colSpan={4} className="bg-sunken px-3 py-1 text-xs text-ink-3">
                      @@ −{h.oldStart},{h.oldLen} +{h.newStart},{h.newLen} @@
                    </td>
                  </tr>
                  {lines.map((l, li) => (
                    <Fragment key={li}>
                      <tr className={rowBg(l.kind)}>
                        {numCell(l.oldNo, "old")}
                        {numCell(l.newNo, "new")}
                        <td className={cx("w-5 text-center align-top select-none", l.kind === "add" ? "text-ok" : l.kind === "del" ? "text-bad" : "text-ink-3")} aria-label={l.kind === "add" ? "added" : l.kind === "del" ? "removed" : undefined}>
                          {sign(l.kind)}
                        </td>
                        <td className="pr-4 whitespace-pre" dangerouslySetInnerHTML={{ __html: l.html || " " }} />
                      </tr>
                      {extras(l.newNo, 4)}
                    </Fragment>
                  ))}
                </tbody>
              ))}
            </table>
          ) : (
            <table className="mono w-full table-fixed border-collapse text-sm leading-6">
              <thead className="sr-only">
                <tr>
                  <th scope="col">Old line</th>
                  <th scope="col">Old code</th>
                  <th scope="col">New line</th>
                  <th scope="col">New code</th>
                </tr>
              </thead>
              {hunks.map(({ h, lines }, hi) => (
                <tbody key={hi}>
                  <tr>
                    <td colSpan={4} className="bg-sunken px-3 py-1 text-xs text-ink-3">
                      @@ −{h.oldStart},{h.oldLen} +{h.newStart},{h.newLen} @@
                    </td>
                  </tr>
                  {toSplitRows(lines).map((row, ri) => {
                    const left = row.left as (HunkLine & { html: string }) | null;
                    const right = row.right as (HunkLine & { html: string }) | null;
                    return (
                      <Fragment key={ri}>
                        <tr>
                          {numCell(left?.oldNo ?? null, "old")}
                          <td className={cx("overflow-hidden pr-3 pl-2 whitespace-pre", left && left.kind === "del" ? "bg-del" : "", !left ? "bg-sunken" : "")} dangerouslySetInnerHTML={{ __html: left?.html || " " }} />
                          {numCell(right?.newNo ?? null, "new")}
                          <td className={cx("overflow-hidden pr-3 pl-2 whitespace-pre", right && right.kind === "add" ? "bg-add" : "", !right ? "bg-sunken" : "")} dangerouslySetInnerHTML={{ __html: right?.html || " " }} />
                        </tr>
                        {extras(right?.newNo ?? null, 4)}
                      </Fragment>
                    );
                  })}
                </tbody>
              ))}
            </table>
          )}
          {threads.filter((t) => !placed.has(t.id)).length ? (
            <div className="flex flex-col gap-2.5 border-t border-line bg-sunken p-3">
              <p className="text-xs text-ink-3">Comments on lines outside the shown hunks</p>
              {threads
                .filter((t) => !placed.has(t.id))
                .map((t) => (
                  <Thread key={t.id} t={t} repo={repo} selected={selected} onSelect={onSelect} />
                ))}
            </div>
          ) : null}
        </div>
      )}
    </section>
  );
}

function Thread({ t, repo, selected, onSelect, anchorText }: { t: ReviewThread; repo: string; selected: string[]; onSelect: (id: string, on: boolean) => void; anchorText?: string }) {
  const router = useRouter();
  const toast = useToast();
  const [reply, setReply] = useState("");
  const [open, setOpen] = useState(!t.resolved);
  const [busy, setBusy] = useState(false);
  const r = { fullName: repo };
  const suggestion = t.comments.find((c) => c.suggestion)?.suggestion;
  const resolveOp = { service: "ChangeService" as const, method: "ResolveThread", request: { repo: r, threadId: t.id, resolved: !t.resolved }, label: t.resolved ? "Reopen thread" : "Resolve thread" };

  async function resolve() {
    setBusy(true);
    try {
      await browserClient().change.resolveThread(resolveOp.request);
      router.refresh();
    } catch (e) {
      toast.error(e);
    } finally {
      setBusy(false);
    }
  }
  async function send() {
    if (!reply.trim()) return;
    setBusy(true);
    try {
      await browserClient().change.replyThread({ repo: r, threadId: t.id, body: reply });
      setReply("");
      router.refresh();
    } catch (e) {
      toast.error(e);
    } finally {
      setBusy(false);
    }
  }

  return (
    <article className={cx("rounded-md border bg-surface font-sans", t.resolved ? "border-line" : "border-line-strong")} aria-label={"Thread on " + pathText(t.path) + " line " + t.startLine}>
      <header className="flex flex-wrap items-center gap-2 border-b border-line px-3 py-1.5 text-sm">
        <button type="button" onClick={() => setOpen((o) => !o)} aria-expanded={open} className="flex min-w-0 items-center gap-1.5 text-ink-2 hover:text-ink">
          <Icon name={open ? "chevronDown" : "chevronRight"} size={14} />
          <span className="mono truncate text-xs">
            {pathText(t.path)}:{t.startLine}
            {t.endLine !== t.startLine ? "–" + t.endLine : ""}
          </span>
        </button>
        <span className="text-xs text-ink-3">r{t.revision}</span>
        {t.resolved ? <Badge tone="ok">resolved</Badge> : <Badge tone="accent">open</Badge>}
        {t.outdated ? <Badge tone="outline">outdated</Badge> : null}
        {suggestion && !t.resolved ? (
          <label className="ml-auto flex items-center gap-1.5 text-xs text-ink-2">
            <input type="checkbox" checked={selected.includes(t.id)} onChange={(e) => onSelect(t.id, e.target.checked)} className="accent-[var(--accent)]" />
            Include in batch
          </label>
        ) : null}
      </header>
      {open ? (
        <div className="flex flex-col gap-3 p-3">
          {t.comments.map((c) => (
            <div key={c.id} className="flex gap-2.5">
              <PrincipalMark principal={c.author} size={22} />
              <div className="min-w-0 flex-1">
                <p className="text-sm">
                  <span className="font-medium">{c.author?.display}</span> <Time iso={c.createdAt} className="text-ink-3" />
                </p>
                <Markdown source={c.body} className="mt-0.5 !text-base" repo={repo} />
                {c.suggestion ? (
                  <div className="mt-2 overflow-hidden rounded-md border border-line">
                    <p className="border-b border-line bg-sunken px-2.5 py-1 text-xs text-ink-2">Suggested change</p>
                    <pre className="mono overflow-x-auto text-sm leading-5">
                      {anchorText ? <span className="block bg-del px-2.5 whitespace-pre">− {anchorText}</span> : null}
                      {c.suggestion.split("\n").map((l, i) => (
                        <span key={i} className="block bg-add px-2.5 whitespace-pre">
                          + {l}
                        </span>
                      ))}
                    </pre>
                  </div>
                ) : null}
              </div>
            </div>
          ))}
          <div className="flex flex-col gap-2">
            <label htmlFor={"reply-" + t.id} className="sr-only">
              Reply
            </label>
            <Textarea id={"reply-" + t.id} value={reply} onChange={(e) => setReply(e.target.value)} placeholder="Reply…" rows={1} />
            <div className="flex items-center gap-1.5">
              <Button size="sm" onClick={send} disabled={!reply.trim() || busy}>
                Reply
              </Button>
              <Button size="sm" variant="ghost" onClick={resolve} disabled={busy} icon={t.resolved ? "refresh" : "check"}>
                {t.resolved ? "Reopen" : "Resolve"}
              </Button>
              <CopyAs compact op={resolveOp} className="ml-auto" />
            </div>
          </div>
        </div>
      ) : null}
    </article>
  );
}

function Composer({ repo, crId, revision, path, line, lineText, onDone }: { repo: string; crId: string; revision: number; path: string; line: number; lineText: string; onDone: () => void }) {
  const router = useRouter();
  const toast = useToast();
  const [body, setBody] = useState("");
  const [suggest, setSuggest] = useState(false);
  const [suggestion, setSuggestion] = useState(lineText);
  const [busy, setBusy] = useState(false);
  const req = { repo: { fullName: repo }, changeRequestId: crId, revision, path: pathBytes(path), side: "new" as const, startLine: line, endLine: line, body, suggestion: suggest ? suggestion : undefined };
  async function submit() {
    setBusy(true);
    try {
      await browserClient().change.createThread(req);
      onDone();
      router.refresh();
    } catch (e) {
      toast.error(e, "Could not comment");
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="flex flex-col gap-2 rounded-md border border-line bg-popover p-3 font-sans">
      <label htmlFor={"compose-" + line} className="text-sm font-medium">
        Comment on line {line}
      </label>
      <Textarea id={"compose-" + line} value={body} onChange={(e) => setBody(e.target.value)} autoFocus placeholder="What should change, and why?" />
      <label className="flex items-center gap-1.5 text-sm text-ink-2">
        <input type="checkbox" checked={suggest} onChange={(e) => setSuggest(e.target.checked)} className="accent-[var(--accent)]" />
        Suggest a replacement for this line
      </label>
      {suggest ? (
        <>
          <label htmlFor={"suggest-" + line} className="sr-only">
            Suggested replacement
          </label>
          <Textarea id={"suggest-" + line} value={suggestion} onChange={(e) => setSuggestion(e.target.value)} className="mono text-sm" />
        </>
      ) : null}
      <div className="flex items-center gap-1.5">
        <Button size="sm" variant="primary" onClick={submit} disabled={busy || (!body.trim() && !suggest)}>
          Comment
        </Button>
        <Button size="sm" variant="ghost" onClick={onDone}>
          Cancel
        </Button>
        <CopyAs compact op={{ service: "ChangeService", method: "CreateThread", request: req, label: "Comment on this line" }} className="ml-auto" />
      </div>
    </div>
  );
}
