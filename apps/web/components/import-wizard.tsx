"use client";

import { useEffect, useState } from "react";
import { browserClient } from "@/lib/api/browser";
import type { FidelityReport, Import, ImportPhase, StartImportRequest } from "@/lib/api/types";
import { compact } from "@/lib/format";
import { CopyAs } from "./copy-as";
import { Icon } from "./icons";
import { Time } from "./time";
import { useToast } from "./toast";
import { Badge, Button, Field, Input, cx } from "./ui";

type Step = "source" | "estimate" | "progress" | "report";
const STEPS: { id: Step; label: string }[] = [
  { id: "source", label: "Source" },
  { id: "estimate", label: "Dry run" },
  { id: "progress", label: "Mirror" },
  { id: "report", label: "Fidelity report" },
];

const STATE_LABEL: Record<string, string> = {
  IMPORT_STATE_QUEUED: "Queued",
  IMPORT_STATE_GIT: "Git: refs, objects, LFS",
  IMPORT_STATE_METADATA: "Metadata: issues, pull requests, reviews",
  IMPORT_STATE_LIVE: "Live: webhook sync",
  IMPORT_STATE_FAILED: "Failed",
  IMPORT_STATE_CANCELLED: "Cancelled",
};

function label(cat: string): string {
  return cat.replace(/_/g, " ");
}

export function ImportWizard({ org }: { org: string }) {
  const toast = useToast();
  const [step, setStep] = useState<Step>("source");
  const [source, setSource] = useState("");
  const [isOrg, setIsOrg] = useState(false);
  const [auth, setAuth] = useState<"app" | "token">("app");
  const [installation, setInstallation] = useState("");
  const [token, setToken] = useState("");
  const [error, setError] = useState("");
  const [imp, setImp] = useState<Import | null>(null);
  const [report, setReport] = useState<FidelityReport | null>(null);
  const [busy, setBusy] = useState(false);

  const start = (phase: ImportPhase): StartImportRequest => ({
    source,
    org: isOrg,
    targetOrg: org,
    phase,
    githubInstallationId: auth === "app" ? installation : undefined,
    githubToken: auth === "token" ? (token ? "••••" : "") : undefined,
  });

  async function run(phase: ImportPhase) {
    setBusy(true);
    setError("");
    try {
      const req = { ...start(phase), githubToken: auth === "token" ? token : undefined };
      const res = await browserClient().import.startImport(req);
      setImp(res);
      setReport(null);
      setStep(phase === "IMPORT_PHASE_DRY_RUN" ? "estimate" : "progress");
    } catch (e) {
      setError((e as Error).message);
      toast.error(e, "Import not started");
    } finally {
      setBusy(false);
    }
  }

  // No polling: while an import runs, the org's event stream says when it moved (import.*), and
  // only then is its status read again.
  const running = !!imp && !imp.liveSince && imp.state !== "IMPORT_STATE_CANCELLED" && imp.state !== "IMPORT_STATE_FAILED";
  const importId = imp?.id ?? "";
  useEffect(() => {
    if (!running || !importId) return;
    const ctl = new AbortController();
    (async () => {
      try {
        for await (const ev of browserClient().event.subscribe({ repo: { fullName: org + "/*" }, fromSeq: "0", kinds: ["import.*"] }, { signal: ctl.signal })) {
          void ev;
          setImp(await browserClient().import.getImport({ id: importId }));
        }
      } catch (e) {
        if (!ctl.signal.aborted) toast.error(e, "Lost the import's event stream");
      }
    })();
    return () => ctl.abort();
  }, [running, importId, org, toast]);

  async function refresh() {
    if (!imp) return;
    try {
      setImp(await browserClient().import.getImport({ id: imp.id }));
    } catch (e) {
      toast.error(e);
    }
  }

  const finished = !!imp?.liveSince;
  useEffect(() => {
    if (!imp || !finished || report) return;
    browserClient()
      .import.getFidelityReport({ id: imp.id })
      .then(setReport)
      .catch((e: unknown) => toast.error(e));
  }, [imp, finished, report, toast]);

  const idx = STEPS.findIndex((s) => s.id === step);

  return (
    <div className="flex flex-col gap-8">
      <ol className="flex flex-wrap gap-2" aria-label="Import steps">
        {STEPS.map((s, i) => (
          <li key={s.id} aria-current={s.id === step ? "step" : undefined} className={cx("flex items-center gap-2 rounded-md border px-2.5 py-1 text-base", s.id === step ? "border-accent bg-accent-soft text-accent-ink" : i < idx ? "border-line text-ink-2" : "border-line text-ink-3")}>
            <span className="num grid size-5 place-items-center rounded-full border border-current text-sm">{i < idx ? <Icon name="check" size={12} /> : i + 1}</span>
            {s.label}
          </li>
        ))}
      </ol>

      {step === "source" ? (
        <form
          className="flex max-w-2xl flex-col gap-4"
          onSubmit={(e) => {
            e.preventDefault();
            void run("IMPORT_PHASE_DRY_RUN");
          }}
        >
          <Field label="GitHub repository or organization" htmlFor="imp-source" hint="owner/repo, or an org name with “whole organization” checked." error={error || undefined}>
            <Input id="imp-source" value={source} onChange={(e) => setSource(e.target.value)} className="mono" placeholder="octo-org/service" />
          </Field>
          <label className="flex items-center gap-1.5 text-base">
            <input type="checkbox" checked={isOrg} onChange={(e) => setIsOrg(e.target.checked)} className="accent-[var(--accent)]" />
            Whole organization
          </label>
          <fieldset className="flex flex-col gap-2">
            <legend className="mb-1 text-base font-medium">Read GitHub with</legend>
            <label className="flex items-start gap-2 text-base">
              <input type="radio" name="auth" checked={auth === "app"} onChange={() => setAuth("app")} className="mt-1 accent-[var(--accent)]" />
              <span>
                GitHub App installation <span className="text-ink-3">(recommended: 3x the rate limit, needed for bridge mode)</span>
              </span>
            </label>
            <label className="flex items-start gap-2 text-base">
              <input type="radio" name="auth" checked={auth === "token"} onChange={() => setAuth("token")} className="mt-1 accent-[var(--accent)]" />
              Personal access token
            </label>
          </fieldset>
          {auth === "app" ? (
            <Field label="Installation id" htmlFor="imp-inst">
              <Input id="imp-inst" value={installation} onChange={(e) => setInstallation(e.target.value)} className="mono" placeholder="from github.com/settings/installations" />
            </Field>
          ) : (
            <Field label="Token" htmlFor="imp-token" hint="Write-only: sent once, never shown again.">
              <Input id="imp-token" type="password" value={token} onChange={(e) => setToken(e.target.value)} autoComplete="off" />
            </Field>
          )}
          <div className="flex items-center gap-1.5">
            <Button type="submit" variant="primary" disabled={busy || !source.trim()}>
              {busy ? "Estimating…" : "Estimate with a dry run"}
            </Button>
            <CopyAs op={{ service: "ImportService", method: "StartImport", request: start("IMPORT_PHASE_DRY_RUN"), label: "Dry-run this import" }} />
          </div>
        </form>
      ) : null}

      {step === "estimate" && imp ? (
        <section aria-labelledby="est-title" className="rounded-lg border border-line bg-surface">
          <header className="mb-4 flex flex-wrap items-center gap-x-3 gap-y-2">
            <h2 id="est-title" className="text-xl font-semibold tracking-tight">
              Dry run · <span className="mono">{imp.source}</span>
            </h2>
            {finished ? <Badge tone="ok">estimate ready</Badge> : <Badge tone="warn">counting…</Badge>}
            <span className="ml-auto text-sm text-ink-3">{imp.rateLimitNote}</span>
          </header>
          {report ? (
            <>
              <FidelityTable report={report} estimate />
              <div className="flex flex-wrap items-center gap-1.5 border-t border-line pt-4 mt-4">
                <Button variant="primary" onClick={() => run("IMPORT_PHASE_MIRROR")} disabled={busy}>
                  Start mirror
                </Button>
                <Button variant="ghost" onClick={() => setStep("source")}>
                  Back
                </Button>
                <CopyAs op={{ service: "ImportService", method: "StartImport", request: start("IMPORT_PHASE_MIRROR"), label: "Start the mirror" }} />
                <p className="w-full text-sm text-ink-3">Mirror keeps GitHub as the source of truth. Bridge and cutover come later and are reversible.</p>
              </div>
            </>
          ) : (
            <Progress imp={imp} />
          )}
        </section>
      ) : null}

      {step === "progress" && imp ? (
        <section aria-labelledby="prog-title" className="rounded-lg border border-line bg-surface">
          <header className="mb-4 flex flex-wrap items-center gap-x-3 gap-y-2">
            <h2 id="prog-title" className="text-xl font-semibold tracking-tight">
              Mirroring <span className="mono">{imp.source}</span>
            </h2>
            <Badge tone={finished ? "ok" : "warn"}>{STATE_LABEL[imp.state] ?? imp.state}</Badge>
            {imp.eta && !finished ? (
              <span className="text-sm text-ink-3">
                done <Time iso={imp.eta} />
              </span>
            ) : null}
            <span className="ml-auto">
              <CopyAs compact op={{ service: "ImportService", method: "GetImport", request: { id: imp.id }, label: "Import status" }} />
            </span>
          </header>
          <Progress imp={imp} />
          <div className="flex items-center gap-1.5 border-t border-line pt-4 mt-4">
            <Button variant="primary" disabled={!report} onClick={() => setStep("report")}>
              {report ? "View fidelity report" : "Report is ready when the mirror is live"}
            </Button>
            {running ? (
              <Button variant="ghost" icon="refresh" onClick={() => void refresh()}>
                Check status
              </Button>
            ) : null}
            {imp.state === "IMPORT_STATE_FAILED" ? <span className="text-base text-bad">The server reports this import as failed (IMPORT_STATE_FAILED).</span> : null}
          </div>
        </section>
      ) : null}

      {step === "report" && report && imp ? (
        <section aria-labelledby="rep-title" className="rounded-lg border border-line bg-surface">
          <header className="mb-4 flex flex-wrap items-center gap-x-3 gap-y-2">
            <h2 id="rep-title" className="text-xl font-semibold tracking-tight">
              Fidelity report · <span className="mono">{imp.source}</span>
            </h2>
            <Badge tone="ok">live since <Time iso={imp.liveSince} /></Badge>
            <span className="ml-auto">
              <CopyAs compact op={{ service: "ImportService", method: "GetFidelityReport", request: { id: imp.id }, label: "Fidelity report" }} />
            </span>
          </header>
          <FidelityTable report={report} />
          <div className="border-t border-line pt-4 mt-4">
            <h3 className="mb-1.5 text-base font-semibold">Before cutover</h3>
            <ul className="flex list-disc flex-col gap-1 pl-5 text-base text-ink-2">
              {report.manualSteps.map((s) => (
                <li key={s}>{s}</li>
              ))}
            </ul>
          </div>
        </section>
      ) : null}
    </div>
  );
}

function Progress({ imp }: { imp: Import }) {
  return (
    <ul className="flex flex-col gap-2.5" data-testid="import-progress">
      {imp.progress.map((p) => {
        const done = Number(p.done);
        const total = Number(p.total);
        const pct = total ? Math.round((done / total) * 100) : 0;
        return (
          <li key={p.category} className="grid grid-cols-[140px_1fr_110px] items-center gap-3 text-base">
            <span className="capitalize">{label(p.category)}</span>
            <span className="h-1.5 overflow-hidden rounded-full bg-sunken" role="progressbar" aria-label={label(p.category)} aria-valuenow={pct} aria-valuemin={0} aria-valuemax={100}>
              <span className={cx("block h-full rounded-full transition-[width] duration-500", pct === 100 ? "bg-ok" : "bg-accent")} style={{ width: pct + "%" }} />
            </span>
            <span className="num text-right text-sm text-ink-2">
              {compact(done)} / {compact(total)}
            </span>
          </li>
        );
      })}
      {imp.progress.length === 0 ? <li className="text-base text-ink-3">Queued…</li> : null}
    </ul>
  );
}

function FidelityTable({ report, estimate = false }: { report: FidelityReport; estimate?: boolean }) {
  const dropped = report.items.reduce((n, i) => n + Number(i.dropped), 0);
  return (
    <div className="overflow-x-auto scrollbar-thin" data-testid="fidelity" tabIndex={0} role="region" aria-label="Fidelity by category">
      <p className="pt-3 text-base text-ink-2">
        {estimate ? "Expected outcome: " : ""}
        {dropped === 0 ? "nothing dropped." : dropped + " item" + (dropped === 1 ? "" : "s") + " cannot be carried over, each with a reason. Nothing is dropped silently."}
      </p>
      <table className="mt-2 w-full min-w-[560px] text-base">
        <thead className="text-left text-sm text-ink-3">
          <tr className="border-y border-line">
            <th scope="col" className="py-2 font-medium">Category</th>
            <th scope="col" className="px-2 py-2 text-right font-medium">{estimate ? "Will import" : "Imported"}</th>
            <th scope="col" className="px-2 py-2 text-right font-medium">Archived</th>
            <th scope="col" className="px-2 py-2 text-right font-medium">Dropped</th>
            <th scope="col" className="py-2 font-medium">Why</th>
          </tr>
        </thead>
        <tbody className="divide-y divide-line">
          {report.items.map((i) => (
            <tr key={i.category} className="align-top">
              <td className="py-2 capitalize">{label(i.category)}</td>
              <td className="num px-2 py-2 text-right">{Number(i.imported).toLocaleString("en-US")}</td>
              <td className="num px-2 py-2 text-right text-ink-2">{i.archived}</td>
              <td className={cx("num px-2 py-2 text-right", Number(i.dropped) ? "text-bad" : "text-ink-3")}>{i.dropped}</td>
              <td className="py-2 text-sm text-ink-2">
                {i.drops.map((d) => (
                  <span key={d.ref} className="block">
                    <span className="mono">{d.ref}</span>: {d.reason}
                  </span>
                ))}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
