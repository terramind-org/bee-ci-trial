"use client";

/**
 * The Copy-as panel: one operation as a CLI command, an HTTP request and an MCP tool call. Its own
 * module because rendering the wire form needs the generated descriptors of every service
 * (`wireOf`, about 200 KB of gzipped script); `copy-as.tsx` loads it only when a dialog opens, so
 * the descriptors stay out of every page's first load.
 */
import * as Tabs from "@radix-ui/react-tabs";
import { useState } from "react";
import { wireOf } from "@/lib/api/ops";
import { httpLine, mcpToolName, toCli, toCurl, toMcp, type Op } from "@/lib/copyas";
import { Icon } from "./icons";
import { buttonClass } from "./ui";

function CopyBlock({ text, label }: { text: string; label: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="relative">
      <pre className="mono max-h-[50vh] overflow-auto rounded-md border border-line bg-sunken p-3 pr-20 text-sm leading-5 whitespace-pre-wrap [overflow-wrap:anywhere] text-ink scrollbar-thin" tabIndex={0} aria-label={label}>
        {text}
      </pre>
      <button
        type="button"
        className={buttonClass("secondary", "sm", "absolute top-2 right-2")}
        onClick={() => {
          void navigator.clipboard?.writeText(text).then(
            () => {
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            },
            () => setCopied(false),
          );
        }}
      >
        <Icon name={copied ? "check" : "copy"} size={14} />
        {copied ? "Copied" : "Copy"}
      </button>
      <span className="sr-only" aria-live="polite">
        {copied ? label + " copied" : ""}
      </span>
    </div>
  );
}

export function CopyAsPanel({ op }: { op: Op }) {
  const wire = wireOf(op.service, op.method, op.request ?? {});
  return (
    <Tabs.Root defaultValue="cli" className="flex flex-col gap-3">
      <Tabs.List aria-label="Format" className="flex gap-1 border-b border-line">
        {[
          ["cli", "CLI"],
          ["http", "HTTP"],
          ["mcp", "MCP"],
        ].map(([v, l]) => (
          <Tabs.Trigger
            key={v}
            value={v as string}
            className="-mb-px h-8 border-b-2 border-transparent px-2.5 text-base text-ink-2 hover:text-ink data-[state=active]:border-accent data-[state=active]:font-medium data-[state=active]:text-ink"
          >
            {l}
          </Tabs.Trigger>
        ))}
      </Tabs.List>
      <Tabs.Content value="cli" className="flex flex-col gap-2">
        <CopyBlock text={toCli(op)} label="CLI command" />
        <p className="text-xs text-ink-3">
          Add <code>--json</code> for one JSON document on stdout; errors carry a stable <code>reason</code>.
        </p>
      </Tabs.Content>
      <Tabs.Content value="http" className="flex flex-col gap-2">
        <p className="mono text-sm text-ink-2">{httpLine(op)}</p>
        <CopyBlock text={toCurl(op)} label="curl request" />
      </Tabs.Content>
      <Tabs.Content value="mcp" className="flex flex-col gap-2">
        <p className="text-sm text-ink-2">
          Tool <code className="mono">{mcpToolName(wire.service, wire.method)}</code> on <code className="mono">bee mcp</code> or <code className="mono">/mcp</code>
        </p>
        <CopyBlock text={toMcp(op)} label="MCP tool call" />
      </Tabs.Content>
    </Tabs.Root>
  );
}
