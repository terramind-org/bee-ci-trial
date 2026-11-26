/**
 * A page as an agent reads it (docs/spec/web-ui.md §2, "Two modes"): the same facts the person's
 * page shows, as plain structured text, then the tools this page offers, each marked read, write
 * or needs a person. One component for every screen, so the agent's view has one shape.
 *
 * A page builds `groups` from the data it already loaded for the person's view and takes `tools`
 * from lib/tools.ts. Nothing is restated by hand: a fact with no value is left out by the page,
 * and a tool that is not in the method table cannot be listed.
 */
import { useId, type ReactNode } from "react";
import type { PageTool } from "@/lib/tools";

export interface Fact {
  label: string;
  value: ReactNode;
}

export interface FactGroup {
  /** The group's heading, lower case: "facts", "needs you", "commits". */
  title: string;
  /** Label and value pairs. */
  facts?: (Fact | null | undefined | false)[];
  /** Or a list, one sentence per line. */
  lines?: ReactNode[];
  /** What to say when the list is empty. */
  empty?: string;
  /** CI keeps passing history behind its count in both modes. */
  collapsed?: boolean;
}

const KIND_NOTE: Record<PageTool["kind"], string> = {
  read: "read",
  write: "write",
  "needs a person": "needs a person",
};

export function PageFacts({ page, title, groups, tools, level = 1 }: { page: string; title: string; groups: FactGroup[]; tools: PageTool[]; /** 2 when the page already has its h1 above (the repository's name). */ level?: 1 | 2 }) {
  // Repository pages can render their own facts and the header facts together.
  const toolsId = useId();
  const Title = level === 1 ? "h1" : "h2";
  const Sub = level === 1 ? "h2" : "h3";
  return (
    <article data-bee-page={page} className="flex max-w-[920px] flex-col gap-6">
      <Title className="font-semibold text-ink"># {title}</Title>
      {groups.map((g) => {
        const facts = (g.facts ?? []).filter((f): f is Fact => !!f);
        const lines = g.lines ?? [];
        const content = (
          <>
            {facts.length ? (
              <dl className="grid grid-cols-1 gap-x-4 sm:grid-cols-[minmax(0,12rem)_minmax(0,1fr)]">
                {facts.map((f) => (
                  <div key={f.label} className="contents">
                    <dt className="text-ink-2">{f.label}</dt>
                    <dd className="min-w-0 [overflow-wrap:anywhere] max-sm:mb-2">{f.value}</dd>
                  </div>
                ))}
              </dl>
            ) : null}
            {lines.length ? (
              <ul>
                {lines.map((l, i) => (
                  <li key={i} className="flex gap-2">
                    <span aria-hidden="true">-</span>
                    <span className="min-w-0 [overflow-wrap:anywhere]">{l}</span>
                  </li>
                ))}
              </ul>
            ) : null}
            {!facts.length && !lines.length ? <p>{g.empty ?? "Nothing."}</p> : null}
          </>
        );
        return (
          <section key={g.title} data-bee-group={g.title}>
            {g.collapsed ? (
              <details>
                <summary className="link min-h-6 cursor-pointer text-ink-2">
                  <Sub className="inline">## {g.title}</Sub>
                </summary>
                <div className="mt-2">{content}</div>
              </details>
            ) : (
              <>
                <Sub className="mb-1 text-ink-2">## {g.title}</Sub>
                {content}
              </>
            )}
          </section>
        );
      })}
      <section data-bee-group="tools" aria-labelledby={toolsId}>
        <Sub id={toolsId} className="mb-1 text-ink-2">
          ## tools on this page
        </Sub>
        <div className="overflow-x-auto scrollbar-thin" tabIndex={0} role="region" aria-labelledby={toolsId}>
          <table className="w-full min-w-[560px] border-collapse text-left">
            <thead>
              <tr className="border-b border-line text-ink-2">
                <th scope="col" className="py-1.5 pr-4 font-normal">
                  Tool
                </th>
                <th scope="col" className="py-1.5 pr-4 font-normal">
                  What it does
                </th>
                <th scope="col" className="py-1.5 font-normal whitespace-nowrap">
                  Kind
                </th>
              </tr>
            </thead>
            <tbody>
              {tools.map((t) => (
                <tr key={t.name} className="border-b border-line align-top" data-tool={t.name} data-kind={t.kind}>
                  <th scope="row" className="py-1.5 pr-4 font-semibold whitespace-nowrap">
                    {t.name}
                  </th>
                  <td className="py-1.5 pr-4">{t.what}</td>
                  <td className="py-1.5 whitespace-nowrap">{t.kind === "needs a person" ? <strong className="font-semibold">{KIND_NOTE[t.kind]}</strong> : <span className="text-ink-2">{KIND_NOTE[t.kind]}</span>}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <p className="mt-3 text-ink-2">
          Each tool is the MCP tool of that name, and the same call over HTTP and the <code>bee</code> CLI. A tool marked needs a person sends the request to the person who answers for you. Do not look for another way.
        </p>
      </section>
    </article>
  );
}
