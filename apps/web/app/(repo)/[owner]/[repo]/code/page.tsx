import Link from "next/link";
import type { Metadata } from "next";
import type { ReactNode } from "react";
import { repositoryWork } from "@/components/agents/work";
import { activityHref } from "@/components/repo/presence";
import { CodeView } from "@/components/code-view";
import { Avatar, toAvatar } from "@/components/avatar";
import { DiffViewer } from "@/components/diff-viewer";
import { Icon } from "@/components/icons";
import { Markdown } from "@/components/markdown";
import { PageMenu } from "@/components/menu";
import { FileTreeRoot, type LastChange } from "@/components/repo/file-tree-root";
import { Time } from "@/components/time";
import { EmptyState, Panel, TabLinks, buttonClass, cx } from "@/components/ui";
import { objectIdText, pathBytes, pathText } from "@/lib/api/ids";
import { type RepoParams, currentPrincipal, scopeFromParams, serverClient } from "@/lib/api/server";
import type { Changeset, Principal, RepoContext } from "@/lib/api/types";
import type { Op } from "@/lib/copyas";
import { bytes } from "@/lib/format";
import { languageOf } from "@/lib/highlight";
import { repoHref } from "@/lib/routes";
import { guard, param, type SearchParams } from "@/lib/page";

export const metadata: Metadata = { title: "Code" };

type View = "code" | "history" | "blame" | "compare";

function codeHref(fullName: string, q: Record<string, string>): string {
  const sp = new URLSearchParams(Object.entries(q).filter(([, v]) => v));
  const s = sp.toString();
  return repoHref(fullName, "code" + (s ? "?" + s : ""));
}

export default async function CodePage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  const sp = await searchParams;
  const path = param(sp, "path").replace(/^\/+|\/+$/g, "");
  const rev = param(sp, "rev") || "main";
  const view = (["code", "history", "blame", "compare"].includes(param(sp, "view")) ? param(sp, "view") : "code") as View;
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName } = await scopeFromParams(params);
    const href = (q: Record<string, string>) => codeHref(fullName, q);
    const refs = await api.ref.listRefs({ repo: ref });
    const at = { expr: rev };

    // Is `path` a directory or a file? Ask the parent listing (trees list restricted entries too).
    let kind: "dir" | "file" | "restricted" = "dir";
    let size = "0";
    if (path) {
      const parent = path.includes("/") ? path.slice(0, path.lastIndexOf("/")) : "";
      const listing = await api.query.listTree({ repo: ref, at, path: pathBytes(parent) });
      const entry = listing.entries.find((e) => pathText(e.path) === path);
      if (entry && entry.mode !== "MODE_TREE") {
        kind = entry.restricted ? "restricted" : "file";
        size = entry.size;
      }
    }

    const crumbs = path ? path.split("/") : [];
    const breadcrumbs = (
      <nav aria-label="Path" className="flex min-w-0 flex-wrap items-center gap-x-1.5 gap-y-0.5 text-md">
        <Link href={href({ rev })} className="mono text-ink-2 hover:text-ink hover:underline">
          {fullName.split("/")[1]}
        </Link>
        {crumbs.map((c, i) => (
          <span key={i} className="flex items-center gap-1">
            <span className="text-ink-3">/</span>
            {i === crumbs.length - 1 ? (
              <span className="mono text-ink" aria-current="page">
                {c}
              </span>
            ) : (
              <Link href={href({ path: crumbs.slice(0, i + 1).join("/"), rev })} className="mono text-ink-2 hover:text-ink hover:underline">
                {c}
              </Link>
            )}
          </span>
        ))}
      </nav>
    );

    const refPicker = (
      <form action={repoHref(fullName, "code")} className="flex items-center gap-1.5">
        <input type="hidden" name="path" value={path} />
        {view !== "code" ? <input type="hidden" name="view" value={view} /> : null}
        <label htmlFor="ref-select" className="sr-only">
          Revision
        </label>
        <select id="ref-select" name="rev" defaultValue={rev} className="mono h-9 max-w-56 rounded-md border border-transparent bg-sunken px-2 text-sm">
          {refs.refs.some((r) => r.name === rev) ? null : <option value={rev}>{rev}</option>}
          {refs.refs.map((r) => (
            <option key={r.name} value={r.name}>
              {r.name}
            </option>
          ))}
        </select>
        <button type="submit" className={buttonClass("secondary", "sm")}>
          Go
        </button>
      </form>
    );

    // The side note is optional: if its context cannot be read, the file view still renders.
    const ctx: RepoContext | null = view !== "compare" && kind !== "restricted" ? await api.repo.getContext({ repo: ref, at, paths: [pathBytes(path)] }).catch(() => null) : null;

    const me = ctx?.activeLeases.length ? await currentPrincipal() : null;
    const work = me ? await repositoryWork(api, ref).catch(() => []) : [];

    let content: ReactNode = null;
    let op: Op = { service: "QueryService", method: "ListTree", request: { repo: ref, at, path: pathBytes(path) }, label: "List this directory" };
    let side: ReactNode = null;

    if (view === "compare") {
      const from = param(sp, "from") || "main~3";
      const to = param(sp, "to") || "main";
      const req = { repo: ref, from: { expr: from }, to: { expr: to }, paths: path ? [pathBytes(path)] : [], includeHunks: true };
      op = { service: "QueryService", method: "Diff", request: req, label: "Diff " + from + " to " + to };
      const items = await api.diffAll(req);
      content = (
        <div className="flex flex-col gap-4">
          <form action={repoHref(fullName, "code")} className="flex flex-wrap items-end gap-3">
            <input type="hidden" name="view" value="compare" />
            <input type="hidden" name="path" value={path} />
            <label className="flex flex-col gap-1 text-sm text-ink-2">
              From
              <input name="from" defaultValue={from} className="mono h-9 w-48 rounded-md border border-transparent bg-sunken px-2 text-sm text-ink" />
            </label>
            <Icon name="arrowRight" className="mb-2.5 text-ink-3" />
            <label className="flex flex-col gap-1 text-sm text-ink-2">
              To
              <input name="to" defaultValue={to} className="mono h-9 w-48 rounded-md border border-transparent bg-sunken px-2 text-sm text-ink" />
            </label>
            <button type="submit" className={buttonClass("primary", "md")}>
              Compare
            </button>
            <p className="w-full text-sm text-ink-3">Revisions: ref names, ref~N, ref@version, or a commit id prefix of at least 10 hex digits.</p>
          </form>
          <DiffViewer repo={fullName} crId={"compare:" + from + ".." + to} items={items} threads={[]} revision={1} latest={1} since={0} readOnly />
        </div>
      );
    } else if (view === "history") {
      const req = { repo: ref, from: at, path: path ? pathBytes(path) : undefined, page: { pageSize: 50 } };
      op = { service: "QueryService", method: "Log", request: req, label: "History of " + (path || "the repository") };
      const log = await api.query.log(req);
      content = (
        <Panel title={"History of " + (path || fullName)} id="history-title">
          {log.changesets.length ? <History changesets={log.changesets} path={path} href={href} /> : <EmptyState title="No history" icon="history" />}
        </Panel>
      );
    } else if (kind === "restricted") {
      content = (
        <EmptyState title="Restricted" icon="lock">
          You can see that <code className="mono">{path}</code> exists, but your token cannot read it. Its id never changes, so hashes still verify.
        </EmptyState>
      );
    } else if (kind === "file" && view === "blame") {
      const req = { repo: ref, at, path: pathBytes(path) };
      op = { service: "QueryService", method: "Blame", request: req, label: "Blame " + path };
      const [blame, file] = await Promise.all([api.query.blame(req), api.readText({ repo: ref, at, path: pathBytes(path) })]);
      const ids = [...new Map(blame.ranges.filter((r) => r.changeset).map((r) => [r.changeset?.raw ?? "", r.changeset])).values()].filter((x) => !!x);
      const sets = await api.query.getChangesets({ repo: ref, ids: ids as NonNullable<(typeof ids)[number]>[], includeMeta: true });
      const byId = new Map(sets.changesets.map((c) => [c.id?.raw ?? "", c]));
      const lines = file.text.split("\n");
      content = (
        <Panel title={"Blame · " + path} id="blame-title">
          <div className="overflow-x-auto rounded-xl bg-sunken scrollbar-thin" tabIndex={0} role="region" aria-label={"Blame of " + path}>
            <table className="mono w-full min-w-[720px] border-collapse text-sm leading-5">
              <thead className="sr-only">
                <tr>
                  <th scope="col">Change</th>
                  <th scope="col">Line</th>
                  <th scope="col">Code</th>
                </tr>
              </thead>
              <tbody>
                {blame.ranges.map((r, i) => {
                  const cs = byId.get(r.changeset?.raw ?? "");
                  return (
                    <tr key={i} className="border-t border-line/60 align-top first:border-t-0">
                      <td className="w-72 px-4 py-1.5 font-sans">
                        <p className="flex items-center gap-1.5 text-sm">
                          <span className="mono text-ink-2">{objectIdText(r.changeset, true)}</span>
                          <Time iso={cs?.meta?.authorTime?.time} className="text-ink-3" />
                        </p>
                        <p className="truncate text-sm" title={cs?.meta?.message}>
                          {cs?.meta?.message}
                        </p>
                        <p className="truncate text-sm text-ink-3">
                          {cs?.meta?.author}
                          {cs?.meta?.provenance?.model ? " · " + cs.meta.provenance.model : ""}
                        </p>
                      </td>
                      <td className="w-12 pr-3 text-right text-ink-3 select-none">
                        {Array.from({ length: r.lineCount }, (_, k) => (
                          <div key={k}>{r.startLine + k}</div>
                        ))}
                      </td>
                      <td className="py-1.5 pr-4 pl-1 whitespace-pre">{lines.slice(r.startLine - 1, r.startLine - 1 + r.lineCount).join("\n")}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </Panel>
      );
    } else if (kind === "file") {
      const req = { repo: ref, at, path: pathBytes(path) };
      op = { service: "QueryService", method: "ReadPath", request: req, label: "Read " + path };
      const file = await api.readText(req);
      content = (
        <Panel
          title={<span className="mono text-md font-normal">{path.split("/").pop()}</span>}
          id="file-title"
          actions={
            <span className="text-sm text-ink-3">
              {languageOf(path)} · {bytes(size)}
            </span>
          }
        >
          {file.binary ? (
            <p className="py-4 text-base text-ink-2">Binary file ({bytes(file.size)}).</p>
          ) : path.endsWith(".md") && param(sp, "raw") !== "1" ? (
            <Markdown source={file.text} repo={fullName} className="max-w-[70ch]" />
          ) : (
            <div className="rounded-xl bg-sunken py-2">
              <CodeView text={file.text} path={path} />
            </div>
          )}
        </Panel>
      );
    } else {
      const req = { repo: ref, at, path: pathBytes(path) };
      const tree = await api.query.listTree(req);
      const readme = tree.entries.find((e) => /(^|\/)readme\.md$/i.test(pathText(e.path)));
      const readmeText = readme ? (await api.readText({ repo: ref, at, path: readme.path })).text : "";
      // What last changed each row. Capped so a directory of hundreds does not fan out into hundreds of reads.
      const rowCap = 40;
      const lasts: Record<string, LastChange | undefined> = {};
      await Promise.all(
        tree.entries.slice(0, rowCap).map(async (e) => {
          const meta = await api.query
            .log({ repo: ref, from: at, path: e.path, firstParent: true, page: { pageSize: 1 } })
            .then((l) => l.changesets[0]?.meta)
            .catch(() => undefined);
          if (meta) lasts[pathText(e.path)] = { title: meta.message.split("\n")[0]?.trim() ?? "", at: meta.authorTime?.time ?? "" };
        }),
      );
      content = (
        <div className="flex flex-col gap-12">
          <FileTreeRoot
            fullName={fullName}
            entries={tree.entries}
            lasts={lasts}
            leases={ctx?.activeLeases ?? []}
            work={work}
            hrefFor={(p) => href({ path: p, rev })}
            limit={Number.POSITIVE_INFINITY}
            empty="This directory is empty."
            label={path ? "Files in " + path : "Files in the repository root"}
          />
          {readme ? (
            <section aria-labelledby="readme-title">
              <h2 id="readme-title" className="mono mb-5 border-t border-line pt-6 text-base font-normal text-ink-2">
                README.md
              </h2>
              <Markdown source={readmeText} repo={fullName} className="max-w-[70ch]" />
            </section>
          ) : null}
        </div>
      );
    }

    if (ctx) {
      side = (
        <div className="flex flex-col gap-7 text-base text-ink-2">
          <h2 id="ctx-title" className="text-xl font-semibold tracking-tight text-ink">
            Before you edit
          </h2>
          <section aria-label="Owners">
            <h3 className="mb-1.5 text-sm font-medium text-ink-3">Owners</h3>
            <p>{ctx.owners.flatMap((o) => o.owners).join(", ") || "No owners; any approval counts"}</p>
          </section>
          <section aria-label="Leases here">
            <h3 className="mb-1.5 text-sm font-medium text-ink-3">Leases here</h3>
            {ctx.activeLeases.length ? (
              <ul className="flex flex-col gap-3">
                {ctx.activeLeases.map((l) => (
                  <li key={l.id} className="flex items-start gap-2.5">
                    <Avatar principal={toAvatar(l.holder)} size={28} />
                    <Link href={activityHref(fullName, work, me ? l.holder : undefined, l)} className="link min-h-6 min-w-0 pt-0.5 [overflow-wrap:anywhere]">
                      <span className="text-ink">{l.holder?.display ?? "Someone"}</span> holds an {l.mode === "LEASE_MODE_EXCLUSIVE" ? "exclusive" : "advisory"} lease on <span className="mono text-sm">{l.paths.map(pathText).join(", ")}</span>.
                    </Link>
                  </li>
                ))}
              </ul>
            ) : (
              <p>None. Take one before you start.</p>
            )}
          </section>
          {ctx.openChangesTouching.length ? (
            <section aria-label="Open pull requests touching this">
              <h3 className="mb-1.5 text-sm font-medium text-ink-3">Open pull requests touching this</h3>
              <ul className="flex flex-col gap-1">
                {ctx.openChangesTouching.slice(0, 6).map((c) => (
                  <li key={c.id}>
                    <Link href={repoHref(fullName, "changes/" + c.id)} className="link">
                      {c.title}
                    </Link>
                  </li>
                ))}
              </ul>
            </section>
          ) : null}
          {ctx.landRules.length ? (
          <section aria-label="Rules for merging into main">
            <h3 className="mb-1.5 text-sm font-medium text-ink-3">Rules for merging into main</h3>
            <ul className="flex list-disc flex-col gap-1 pl-5">
              {ctx.landRules.map((r) => (
                <li key={r}>{r}</li>
              ))}
            </ul>
          </section>
          ) : null}
        </div>
      );
    }

    const isFile = kind === "file";
    return (
      <>
        <header className="mt-8 mb-6 flex flex-col gap-3 md:flex-row md:items-center md:justify-between">
          {breadcrumbs}
          <div className="flex flex-wrap items-center gap-2">
            {refPicker}
            <PageMenu op={op} />
          </div>
        </header>
        <TabLinks
          label="Code view"
          items={[
            { href: href({ path, rev }), label: isFile ? "File" : "Tree", active: view === "code" },
            { href: href({ path, rev, view: "history" }), label: "History", active: view === "history" },
            ...(isFile ? [{ href: href({ path, rev, view: "blame" }), label: "Blame", active: view === "blame" }] : []),
            { href: href({ path, view: "compare" }), label: "Compare", active: view === "compare" },
          ]}
        />
        <div className={cx("grid grid-cols-1 gap-x-16 gap-y-12", side ? "xl:grid-cols-[minmax(0,1fr)_280px]" : "")}>
          <div className="min-w-0">{content}</div>
          {side ? <aside aria-label="Path context" className="min-w-0">{side}</aside> : null}
        </div>
      </>
    );
  });
}

function History({ changesets, path, href }: { changesets: Changeset[]; path: string; href: (q: Record<string, string>) => string }) {
  return (
    <ol className="-mx-3 flex flex-col">
      {changesets.map((c) => {
        const author: Principal = { id: c.meta?.creator ?? "", kind: c.meta?.provenance ? "PRINCIPAL_KIND_AGENT" : "PRINCIPAL_KIND_HUMAN", display: c.meta?.author ?? "", ownerId: "", runtime: c.meta?.provenance?.runtime.split("/")[0] ?? "", externalIds: [] };
        const id = objectIdText(c.id);
        return (
          <li key={id} className="flex flex-col gap-1 rounded-lg px-3 py-2.5 hover:bg-hover md:flex-row md:items-center md:gap-6">
            <div className="flex min-w-0 flex-1 items-start gap-3">
              <Avatar principal={toAvatar(author)} size={28} />
              <div className="min-w-0">
                <p className="truncate text-md">{c.meta?.message}</p>
                <p className="flex flex-wrap gap-x-2 text-base text-ink-3">
                  <span>{author.display}</span>
                  {c.meta?.provenance ? <span>{c.meta.provenance.model}</span> : null}
                  <span>
                    {c.delta.length} path{c.delta.length === 1 ? "" : "s"}
                  </span>
                </p>
              </div>
            </div>
            <div className="flex items-center gap-4 text-base">
              <Link href={href({ view: "compare", from: id.slice(0, 16) + "~1", to: id.slice(0, 16), path })} className="mono text-sm text-accent-ink hover:underline">
                {objectIdText(c.id, true)}
              </Link>
              <Time iso={c.meta?.authorTime?.time} className="w-20 text-right text-ink-3" />
            </div>
          </li>
        );
      })}
    </ol>
  );
}
