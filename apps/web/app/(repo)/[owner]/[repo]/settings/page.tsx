import type { Metadata } from "next";
import type { ReactNode } from "react";
import { PageFacts, type FactGroup } from "@/components/bee/page-facts";
import { ButtonLink } from "@/components/kit";
import { PageMenu } from "@/components/menu";
import {
  accessSentences, collaboratorSentence, InstructionContent, InstructionOrigin, INSTRUCTIONS_CHANGE,
  keySentence, moreLinks, SECTIONS, SettingLinks, SettingRows, teamSentence,
} from "@/components/settings/details";
import { policyRules, Rules, RULES_CHANGE_NOTE } from "@/components/settings/rules";
import { VisibilitySettings } from "@/components/settings/visibility";
import { VISIBILITIES } from "@/components/settings/visibility-info";
import { PageHeader } from "@/components/ui";
import { pathBytes } from "@/lib/api/ids";
import type { BeeClient } from "@/lib/api/client";
import { type RepoParams, scopeFromParams, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { PageResponse, Policy, RepoRef } from "@/lib/api/types";
import { guard, param, type SearchParams } from "@/lib/page";
import { repoHref } from "@/lib/routes";
import { pageTools } from "@/lib/tools";
import { requestMode } from "../../../../request-mode";

export const metadata: Metadata = { title: "Settings" };

const POLICY_PATH = ".beehive/policy.toml";

type RefRequest = { repo: RepoRef };

async function readPolicy(api: BeeClient, req: RefRequest, defaultRef: string): Promise<{ policy: Policy; unavailable?: { reason: string; message: string } }> {
  try {
    return { policy: await api.policy.getPolicy(req) };
  } catch (err) {
    if (!(err instanceof BeeError) || err.code !== "unimplemented") throw err;
    const file = await api.readText({ ...req, at: { expr: defaultRef }, path: pathBytes(POLICY_PATH) }).catch((e: unknown) => {
      if (e instanceof BeeError && e.code === "not_found") return { text: "" };
      throw e;
    });
    return {
      policy: { path: POLICY_PATH, source: file.text, epoch: "", normalizedJson: "" },
      unavailable: { reason: err.reason, message: err.message },
    };
  }
}

/** Read errors belong to the relevant section, so restricted deploy keys cannot hide access. */
async function sectionData<T>(enabled: boolean, load: () => Promise<T>): Promise<{ value?: T; error?: string }> {
  if (!enabled) return {};
  try {
    return { value: await load() };
  } catch (err) {
    if (!(err instanceof BeeError)) throw err;
    return { error: err.reason + " · " + err.message };
  }
}

async function readAll<T>(load: (pageToken?: string) => Promise<{ rows: T[]; page?: PageResponse }>) {
  const rows: T[] = [];
  let token: string | undefined;
  do {
    const page = await load(token);
    rows.push(...page.rows);
    token = page.page?.nextPageToken;
  } while (token);
  return rows;
}

function renderRules(rules: ReturnType<typeof policyRules>) {
  return rules.map(({ text, raw }) => raw ? <code key={text} className="whitespace-pre-wrap">{text}</code> : text);
}

function SectionError({ message }: { message: string }) {
  return <p className="text-sm text-ink-2 [overflow-wrap:anywhere]">{message}</p>;
}

export default async function SettingsPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  return guard(async () => {
    const [api, { ref, fullName, owner }, query, { mode }] = await Promise.all([
      serverClient(), scopeFromParams(params), searchParams, requestMode(),
    ]);
    const active = SECTIONS.find(({ id }) => id === param(query, "section")) ?? SECTIONS[0];
    const agent = mode === "bee";
    const req = { repo: ref };
    const repo = await api.repo.getRepo(req);
    const defaultRef = repo.defaultRef || "main";

    const showAccess = agent || active.id === "access";
    const [rules, instructions, collaborators, keys, grants] = await Promise.all([
      sectionData(showAccess || active.id === "rules", () => readPolicy(api, req, defaultRef)),
      sectionData(agent || active.id === "instructions", () => api.agent.resolveAgentInstructions({ ...req, revision: defaultRef })),
      sectionData(showAccess, () => readAll(async (pageToken) => {
        const { collaborators, page } = await api.repo.listCollaborators({ ...req, page: { pageToken } });
        return { rows: collaborators, page };
      })),
      sectionData(showAccess, () => api.repo.listDeployKeys(req)),
      sectionData(showAccess, () => readAll(async (pageToken) => {
        const { grants, page } = await api.team.listRepoTeams({ ...req, page: { pageToken } });
        return { rows: grants, page };
      })),
    ]);

    const ruleLines = policyRules(rules.value?.policy.normalizedJson);
    const files = instructions.value?.files ?? [];
    const people = (collaborators.value ?? []).flatMap((row) => collaboratorSentence(row) ?? []);
    const teams = await sectionData(Boolean(grants.value?.length), () => api.team.listTeams({ org: owner }));
    const teamRoles = (grants.value ?? []).map((grant) => teamSentence(grant, teams.value?.teams ?? []));
    const access = accessSentences(rules.value?.policy.normalizedJson ?? "");
    const deployKeys = keys.value?.keys.map(keySentence) ?? [];
    const agentTokens = <ButtonLink href={repoHref(fullName, "agents")} variant="ghost" size="sm" className="underline">Agent tokens</ButtonLink>;
    const more = moreLinks(fullName, owner);
    const visibility = repo.visibility === "VISIBILITY_UNSPECIFIED" ? null : VISIBILITIES[repo.visibility];
    const origin = (file: (typeof files)[number]) => <InstructionOrigin file={file} repo={fullName} rev={defaultRef} />;

    if (agent) {
      const groups: FactGroup[] = [
        {
          title: "rules",
          lines: [
            ...renderRules(ruleLines),
            ...(rules.value?.unavailable && rules.value.policy.source ? [<pre key="source" className="whitespace-pre-wrap">{rules.value.policy.source}</pre>] : []),
            ...[rules.error, rules.value?.unavailable?.message].filter(Boolean),
            RULES_CHANGE_NOTE,
          ],
        },
        {
          title: "instructions",
          facts: files.map((file) => ({
            label: file.path || "Organization instructions",
            value: <>
              {file.path ? <InstructionOrigin file={file} repo={fullName} rev={defaultRef} linkLabel="Open in Code" /> : null}
              <pre className="whitespace-pre-wrap">{file.content}</pre>
            </>,
          })),
          lines: [...(instructions.error ? [instructions.error] : files.length ? [] : ["No instructions resolve at the repository root."]), INSTRUCTIONS_CHANGE],
        },
        {
          title: "access",
          lines: [
            ...people, ...teamRoles, ...access,
            ...[collaborators.error, grants.error, teams.error, rules.error].filter(Boolean),
            ...deployKeys, ...(keys.error ? [keys.error] : deployKeys.length ? [] : ["No deploy keys."]), agentTokens,
          ],
        },
        {
          title: "visibility",
          facts: [{ label: "current", value: visibility?.label ?? "The server did not report visibility." }],
          lines: [
            "Public. " + VISIBILITIES.VISIBILITY_PUBLIC.description,
            "Private. " + VISIBILITIES.VISIBILITY_PRIVATE.description,
            ...(repo.visibility === "VISIBILITY_INTERNAL" ? ["Internal. " + VISIBILITIES.VISIBILITY_INTERNAL.description] : []),
          ],
        },
        {
          title: "more settings",
          lines: more.map((item) => <span key={item.href}><ButtonLink href={item.href} size="sm" variant="ghost" className="underline">{item.label}</ButtonLink> {item.description}</span>),
        },
      ];
      return <PageFacts page="settings" title={fullName + " · settings"} groups={groups} tools={pageTools("settings")} />;
    }

    let detail: ReactNode;
    switch (active.id) {
      case "rules":
        detail = rules.value
          ? <Rules repo={fullName} policy={rules.value.policy} defaultRef={defaultRef} unavailable={rules.value.unavailable} />
          : <><SectionHeading>Rules</SectionHeading>{rules.error ? <SectionError message={rules.error} /> : null}</>;
        break;
      case "instructions":
        detail = (
          <>
            <SectionHeading>Instructions</SectionHeading>
            {instructions.error ? <SectionError message={instructions.error} /> : null}
            {!instructions.error && !files.length ? <p>No instructions resolve at the repository root.</p> : null}
            {files.map((file) => (
              <article key={file.path || file.name} className="space-y-3 border-b border-line py-4">
                <h3 className="text-base font-medium">{origin(file)}</h3>
                <InstructionContent file={file} repo={fullName} />
              </article>
            ))}
            <p className="text-sm text-ink-2">{INSTRUCTIONS_CHANGE}</p>
          </>
        );
        break;
      case "access":
        detail = (
          <>
            <SectionHeading>Access</SectionHeading>
            <p className="text-ink-2">Repository roles are subject to path rules and approval requirements.</p>
            <SettingRows items={people} />
            {collaborators.error ? <SectionError message={collaborators.error} /> : null}
            {teamRoles.length ? <div><h3 className="text-base font-medium">Teams</h3><SettingRows items={teamRoles} /></div> : null}
            {grants.error || teams.error ? <SectionError message={grants.error ?? teams.error ?? ""} /> : null}
            {access.length ? <div><h3 className="text-base font-medium">Path rules</h3><SettingRows items={access} /></div> : null}
            {rules.error ? <SectionError message={rules.error} /> : null}
            <div>
              <h3 className="mb-2 text-base font-medium">Deploy keys</h3>
              {keys.error ? <SectionError message={keys.error} /> : deployKeys.length ? <SettingRows items={deployKeys} /> : <p className="text-ink-2">No deploy keys.</p>}
            </div>
            {agentTokens}
          </>
        );
        break;
      case "visibility":
        detail = <><SectionHeading>Visibility</SectionHeading><VisibilitySettings repo={fullName} visibility={repo.visibility} version={repo.version ?? "0"} /></>;
        break;
      case "more":
        detail = (
          <>
            <SectionHeading>More settings</SectionHeading>
            <SettingRows items={more.map((item) => (
              <div key={item.href} className="flex flex-col items-start gap-1 sm:flex-row sm:items-center sm:justify-between">
                <ButtonLink href={item.href} size="sm" variant="ghost" className="underline">{item.label}</ButtonLink>
                <span className="text-sm text-ink-2">{item.description}</span>
              </div>
            ))} />
          </>
        );
        break;
    }
    return (
      <>
        <PageHeader title="Settings" actions={active.id === "rules" && rules.value && !rules.value.unavailable ? <PageMenu op={{ service: "PolicyService", method: "GetPolicy", request: req, label: "Show the effective rules" }} /> : null} />
        <div className="grid gap-8 md:grid-cols-[10rem_minmax(0,1fr)]">
          <SettingLinks repo={fullName} active={active.id} />
          <section aria-label={active.label} className="min-w-0 max-w-3xl space-y-5" data-settings-section={active.id}>
            {detail}
          </section>
        </div>
      </>
    );
  });
}

function SectionHeading({ children }: { children: ReactNode }) {
  return <h2 className="text-xl font-light">{children}</h2>;
}
