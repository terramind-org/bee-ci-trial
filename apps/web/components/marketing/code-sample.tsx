"use client";

import { useEffect, useState } from "react";
import { Icon } from "@/components/icons";
import { Button } from "@/components/ui";

/** A block of copyable text with a Copy button; the text is selected for manual copy if the clipboard is blocked. */
export function CodeSample({ label, text }: { label: string; text: string }) {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const t = setTimeout(() => setCopied(false), 1500);
    return () => clearTimeout(t);
  }, [copied]);
  return (
    <div className="flex min-w-0 flex-col rounded-lg border border-line bg-surface">
      <div className="flex h-10 items-center justify-between border-b border-line pl-3.5 pr-1.5">
        <h3 className="text-base font-medium">{label}</h3>
        <Button
          size="sm"
          variant="ghost"
          aria-label={"Copy the " + label + " example"}
          onClick={() => {
            void navigator.clipboard?.writeText(text).then(
              () => setCopied(true),
              () => setCopied(false),
            );
          }}
        >
          <Icon name={copied ? "check" : "copy"} size={14} />
          {copied ? "Copied" : "Copy"}
        </Button>
      </div>
      <pre tabIndex={0} aria-label={label + " example"} className="mono max-h-96 grow overflow-auto p-3.5 text-base leading-5 whitespace-pre-wrap text-ink [overflow-wrap:anywhere] scrollbar-thin">
        {text}
      </pre>
      <span className="sr-only" aria-live="polite">
        {copied ? label + " example copied" : ""}
      </span>
    </div>
  );
}
