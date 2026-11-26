"use client";

import { useState } from "react";
import { CopyAs } from "@/components/copy-as";
import { State } from "@/components/parts";
import { CodeSample } from "@/components/marketing/code-sample";
import { PromptCard } from "@/components/prompt-card";
import { Button, ButtonLink, Field, Input, Select, Textarea, cx } from "@/components/ui";
import { browserClient } from "@/lib/api/browser";
import type { Edition } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { CloneUrls, Repo, Visibility } from "@/lib/api/types";
import type { Op } from "@/lib/copyas";
import { repoHref } from "@/lib/routes";

type Step = "owner" | "repo" | "agent";
const STEPS: { id: Step; label: string }[] = [
  { id: "owner", label: "Owner" },
  { id: "repo", label: "Repository" },
  { id: "agent", label: "Connect your agent" },
];

export interface OwnerChoice {
  name: string;
  display: string;
}

/** The server's reason and message, as it said them. */
function reasonOf(e: unknown): string {
  return e instanceof BeeError ? e.reason + ": " + e.message : String(e);
}

const NEW = "new";
const PERSONAL = "personal";

/**
 * `/new`: pick an owner (an organization you belong to, your personal namespace, or a new
 * organization), create the repository, then connect an agent to it. Every step that changes
 * something says so and has Copy as. A refusal (a policy, a taken name) shows the server's reason.
 */
export function OnboardingWizard({ login, orgs: initialOrgs, endpoint, edition }: { login: string | null; orgs: OwnerChoice[]; endpoint: string; edition: Edition }) {
  const [step, setStep] = useState<Step>("owner");
  const [orgs, setOrgs] = useState(initialOrgs);
  const personalMissing = !!login && !orgs.some((o) => o.name === login);
  const [choice, setChoice] = useState(orgs[0] ? "org:" + orgs[0].name : personalMissing ? PERSONAL : NEW);
  const [orgName, setOrgName] = useState("");
  const [orgDisplay, setOrgDisplay] = useState("");
  const [owner, setOwner] = useState("");

  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [visibility, setVisibility] = useState<Visibility>("VISIBILITY_PRIVATE");
  const [readme, setReadme] = useState(true);

  const [repo, setRepo] = useState<Repo | null>(null);
  const [clone, setClone] = useState<CloneUrls | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const idx = STEPS.findIndex((s) => s.id === step);

  const orgOp = (): Op | null => {
    if (choice === PERSONAL && login) return { service: "OrgService", method: "CreateOrg", request: { name: login, display: login }, label: "Create your personal namespace" };
    if (choice === NEW) return { service: "OrgService", method: "CreateOrg", request: { name: orgName.trim(), ...(orgDisplay.trim() ? { display: orgDisplay.trim() } : {}) }, label: "Create the organization" };
    return null;
  };
  const chosenOwner = choice.startsWith("org:") ? choice.slice(4) : choice === PERSONAL ? (login ?? "") : orgName.trim();
  const repoReq = { org: chosenOwner || owner, name: name.trim(), visibility, description: description.trim() || undefined, autoInit: readme };
  const repoOp: Op = { service: "RepoService", method: "CreateRepo", request: repoReq, label: "Create the repository" };

  async function chooseOwner() {
    setBusy(true);
    setError("");
    try {
      const op = orgOp();
      if (op) {
        const made = await browserClient().org.createOrg(op.request as { name: string; display?: string });
        setOrgs((o) => [...o, { name: made.name, display: made.display }]);
        setChoice("org:" + made.name);
        setOwner(made.name);
      } else {
        setOwner(chosenOwner);
      }
      setStep("repo");
    } catch (e) {
      setError(reasonOf(e));
    } finally {
      setBusy(false);
    }
  }

  async function createRepo() {
    setBusy(true);
    setError("");
    try {
      const api = browserClient();
      const made = await api.repo.createRepo({ ...repoReq, org: owner });
      setRepo(made);
      // The clone URLs come from the repository's home; without them the step says so.
      setClone(await api.repo.getRepoHome({ fullName: made.fullName }).then((h) => h.cloneUrls, () => null));
      setStep("agent");
    } catch (e) {
      setError(reasonOf(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div data-testid="onboarding" className="flex flex-col gap-8">
      <ol className="flex flex-wrap gap-x-6 gap-y-2 border-b border-line pb-4" aria-label="Steps">
        {STEPS.map((s, i) => (
          <li key={s.id} aria-current={s.id === step ? "step" : undefined}>
            <State shape={i < idx ? "passed" : i === idx ? "running" : "open"} className={cx("text-sm", i !== idx && "text-ink-3")}>{s.label}</State>
          </li>
        ))}
      </ol>

      {step === "owner" ? (
        <form
          className="flex max-w-2xl flex-col gap-4"
          onSubmit={(e) => {
            e.preventDefault();
            void chooseOwner();
          }}
        >
          <h2 className="text-xl font-light tracking-tight">Who will own it?</h2>
          <fieldset className="flex flex-col gap-1.5">
            <legend className="sr-only">Owner</legend>
            {orgs.map((o) => (
              <Radio key={o.name} value={"org:" + o.name} choice={choice} onChange={setChoice} label={o.display || o.name} hint={o.name === login ? "Your personal namespace, " + o.name : o.name} />
            ))}
            {personalMissing ? <Radio value={PERSONAL} choice={choice} onChange={setChoice} label={"Personal: " + login} hint={"Creates your own namespace, " + login} /> : null}
            <Radio value={NEW} choice={choice} onChange={setChoice} label="A new organization" hint="For a team, a company or a project" />
          </fieldset>
          {choice === NEW ? (
            <div className="grid gap-3 sm:grid-cols-2">
              <Field label="Organization name" htmlFor="org-name" hint="Lowercase letters, digits and hyphens. It becomes the first part of every repository URL.">
                <Input id="org-name" value={orgName} onChange={(e) => setOrgName(e.target.value)} autoComplete="off" spellCheck={false} placeholder="acme" />
              </Field>
              <Field label="Display name" htmlFor="org-display" hint="Optional.">
                <Input id="org-display" value={orgDisplay} onChange={(e) => setOrgDisplay(e.target.value)} autoComplete="off" placeholder="Acme Inc." />
              </Field>
            </div>
          ) : null}
          {error ? (
            <p role="alert" className="text-base text-bad [overflow-wrap:anywhere]">
              {error}
            </p>
          ) : null}
          <div className="flex flex-wrap items-center gap-2">
            <Button type="submit" variant="primary" disabled={busy || (choice === NEW && !orgName.trim())}>
              {busy ? "Working…" : orgOp() ? "Create and continue" : "Continue"}
            </Button>
            {orgOp() ? <CopyAs op={orgOp()!} /> : null}
          </div>
        </form>
      ) : null}

      {step === "repo" ? (
        <form
          className="flex max-w-2xl flex-col gap-4"
          onSubmit={(e) => {
            e.preventDefault();
            void createRepo();
          }}
        >
          <h2 className="text-xl font-light tracking-tight">
            Name the repository <span className="text-base font-normal text-ink-3">under {owner}</span>
          </h2>
          <Field label="Repository name" htmlFor="repo-name" hint="Letters, digits, dots, underscores and hyphens.">
            <Input id="repo-name" value={name} onChange={(e) => setName(e.target.value)} autoComplete="off" spellCheck={false} placeholder="api" />
          </Field>
          <Field label="Description" htmlFor="repo-desc" hint="Optional.">
            <Textarea id="repo-desc" value={description} onChange={(e) => setDescription(e.target.value)} rows={2} />
          </Field>
          <Field label="Visibility" htmlFor="repo-vis" hint={visibility === "VISIBILITY_PUBLIC" ? "Anyone can read it, signed in or not." : visibility === "VISIBILITY_INTERNAL" ? "Everyone in the instance can read it." : "Only people and agents you grant access to."}>
            <Select id="repo-vis" value={visibility} onChange={(e) => setVisibility(e.target.value as Visibility)}>
              <option value="VISIBILITY_PRIVATE">Private</option>
              <option value="VISIBILITY_PUBLIC">Public</option>
              {edition === "enterprise" ? <option value="VISIBILITY_INTERNAL">Internal</option> : null}
            </Select>
          </Field>
          <label className="flex min-h-6 items-center gap-2 text-base">
            <Input type="checkbox" className="size-4 shrink-0 p-0 accent-ink shadow-none" checked={readme} onChange={(e) => setReadme(e.target.checked)} /> Add a README as the first commit
          </label>
          {error ? (
            <p role="alert" className="text-base text-bad [overflow-wrap:anywhere]">
              {error}
            </p>
          ) : null}
          <div className="flex flex-wrap items-center gap-2">
            <Button type="submit" variant="primary" disabled={busy || !name.trim()}>
              {busy ? "Creating…" : "Create repository"}
            </Button>
            <Button
              variant="ghost"
              onClick={() => {
                setError("");
                setStep("owner");
              }}
            >
              Back
            </Button>
            <CopyAs op={{ ...repoOp, request: { ...repoReq, org: owner } }} />
          </div>
        </form>
      ) : null}

      {step === "agent" && repo ? (
        <div className="flex flex-col gap-4">
          <div className="max-w-3xl">
            <h2 className="text-xl font-light tracking-tight">
              <span>{repo.fullName}</span> is ready
            </h2>
            <p className="mt-1 text-base text-ink-2">Clone it, or hand an agent the prompt below. The prompt carries this server&apos;s address and the repository name.</p>
          </div>
          <div className="grid max-w-3xl gap-3 md:grid-cols-2">
            {clone ? (
              <>
                <CodeSample label="Clone over HTTPS" text={"git clone " + clone.https} />
                {clone.ssh ? <CodeSample label="Clone over SSH" text={"git clone " + clone.ssh} /> : null}
              </>
            ) : (
              <p className="py-2 text-base text-ink-2 md:col-span-2">The clone addresses could not be read from this server. Open the repository to find them.</p>
            )}
          </div>
          <div className="max-w-3xl">
            <PromptCard id="setup-repo" vars={{ endpoint, repo: repo.fullName }} />
          </div>
          <div>
            <ButtonLink href={repoHref(repo.fullName)} variant="primary">
              Go to repository
            </ButtonLink>
          </div>
        </div>
      ) : null}
    </div>
  );
}

function Radio({ value, choice, onChange, label, hint }: { value: string; choice: string; onChange: (v: string) => void; label: string; hint: string }) {
  const on = value === choice;
  return (
    <label className={cx("flex cursor-pointer items-start gap-2.5 border-b border-line px-1 py-2.5", on ? "text-ink" : "text-ink-2 hover:bg-hover")}>
      <Input type="radio" name="owner" value={value} checked={on} onChange={() => onChange(value)} className="mt-1 size-4 shrink-0 p-0 accent-ink shadow-none" />
      <span className="min-w-0">
        <span className="block text-base font-medium">{label}</span>
        <span className="block text-sm text-ink-3 [overflow-wrap:anywhere]">{hint}</span>
      </span>
    </label>
  );
}
