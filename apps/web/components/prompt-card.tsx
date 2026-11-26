"use client";

import { useEffect, useId, useMemo, useRef, useState } from "react";
import { promptFor, getPrompt, type PromptVars } from "@/lib/prompts";
import { Icon } from "./icons";
import { useToast } from "./toast";
import { Button, cx } from "./ui";

/**
 * One agent prompt: its title, when to use it, a collapsible mono body and a Copy button. The
 * text is the prompt with {{endpoint}}, {{repo}} and {{org}} already filled from `vars`.
 * `compact` is the tighter form for embedding in other pages: a one-line "when" and less padding.
 * Outside compact, the card is the target of `/prompts#<id>` links.
 */
export function PromptCard({ id, vars, compact = false }: { id: string; vars: PromptVars; compact?: boolean }) {
  const prompt = getPrompt(id);
  const { endpoint, repo, org } = vars;
  const text = useMemo(() => (prompt ? promptFor(id, { endpoint, repo, org }) : ""), [prompt, id, endpoint, repo, org]);
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const [reveal, setReveal] = useState(false);
  const toast = useToast();
  const bodyId = useId();
  const body = useRef<HTMLPreElement>(null);

  // Copy blocked (an insecure page, a denied permission): show the text selected, so the
  // person can copy it by hand.
  useEffect(() => {
    if (!reveal || !open || !body.current) return;
    const range = document.createRange();
    range.selectNodeContents(body.current);
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
    body.current.focus();
    setReveal(false);
  }, [reveal, open]);

  useEffect(() => {
    if (!copied) return;
    const t = setTimeout(() => setCopied(false), 1500);
    return () => clearTimeout(t);
  }, [copied]);

  if (!prompt) return null;

  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      toast.push({ tone: "ok", title: "Prompt copied", detail: prompt!.title });
    } catch {
      setOpen(true);
      setReveal(true);
      toast.push({ tone: "neutral", title: "Copy is blocked here", detail: "The prompt is selected: press Ctrl+C or ⌘C." });
    }
  }

  const lines = text.split("\n").length;
  return (
    <article id={compact ? undefined : id} className={cx(compact ? "rounded-lg border border-line bg-surface" : "rule scroll-mt-16 pt-4")}>
      <header className={cx("flex items-start gap-3", compact ? "px-3 py-2.5" : "pb-2")}>
        <div className="min-w-0 flex-1">
          <h3 className={cx("font-medium [overflow-wrap:anywhere]", compact ? "text-base" : "text-lg")}>{prompt.title}</h3>
          <p className={cx("mt-0.5 text-base text-ink-2", compact && "truncate")} title={compact ? prompt.when : undefined}>
            {prompt.when}
          </p>
        </div>
        <Button size="sm" icon={copied ? "check" : "copy"} onClick={copy} aria-label={"Copy prompt: " + prompt.title} className="shrink-0">
          {copied ? "Copied" : "Copy"}
        </Button>
      </header>
      <div className={cx(compact && "border-t border-line")}>
        <button
          type="button"
          aria-expanded={open}
          aria-controls={bodyId}
          onClick={() => setOpen((o) => !o)}
          className={cx("flex h-9 w-full items-center gap-1.5 text-base text-ink-2 transition-colors hover:bg-hover hover:text-ink", compact && "px-3")}
        >
          <Icon name={open ? "chevronDown" : "chevronRight"} size={14} />
          {open ? "Hide the prompt" : "Show the prompt"}
          <span className="ml-auto text-sm text-ink-3">{lines} lines</span>
        </button>
        <pre
          ref={body}
          id={bodyId}
          hidden={!open}
          tabIndex={0}
          aria-label={prompt.title + ", prompt text"}
          className={cx(
            "mono overflow-auto rounded-md bg-sunken text-sm leading-5 whitespace-pre-wrap text-ink [overflow-wrap:anywhere] scrollbar-thin",
            compact ? "max-h-[50vh] p-3" : "max-h-[60vh] p-3.5",
          )}
        >
          {text}
        </pre>
      </div>
    </article>
  );
}
