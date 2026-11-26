import type { ReactNode } from "react";
import { ButtonLink, TabLinks } from "@/components/kit";
import { renderMarkdown } from "@/components/markdown";
import type { Res } from "@/lib/api/methods";
import type { Team } from "@/lib/api/types";
import { listing } from "@/lib/checkwords";
import { orgSettingsHref, repoHref } from "@/lib/routes";

export const SECTIONS = [
  { id: "rules", label: "Rules" },
  { id: "instructions", label: "Instructions" },
  { id: "access", label: "Access" },
  { id: "visibility", label: "Visibility" },
  { id: "more", label: "More settings" },
] as const;

export type SettingsSection = (typeof SECTIONS)[number]["id"];

export function SettingLinks({ repo, active }: { repo: string; active: SettingsSection }) {
  return (
    <TabLinks
      sidebar
      label="Settings sections"
      items={SECTIONS.map(({ id, label }) => ({
        label,
        href: repoHref(repo, id === "rules" ? "settings" : "settings?section=" + id),
        active: id === active,
      }))}
    />
  );
}

function fileHref(repo: string, rev: string, path: string) {
  return repoHref(repo, "code?" + new URLSearchParams({ path, rev }));
}

type Instruction = Res<"AgentService", "ResolveAgentInstructions">["files"][number];

export function InstructionOrigin({ file, repo, rev, linkLabel }: { file: Instruction; repo: string; rev: string; linkLabel?: string }) {
  return (
    <>
      {file.path ? <ButtonLink href={fileHref(repo, rev, file.path)} size="sm" variant="ghost" className="mono underline">{linkLabel ?? file.path}</ButtonLink> : file.source === "AGENT_INSTRUCTION_SOURCE_ORG" ? "Organization instructions" : file.name}
      {file.appliesTo.length > 0 ? <span> · Applies to {listing(file.appliesTo)}</span> : null}
      {file.includes.length > 0 ? <span> · Includes {file.includes.map((path) => (
        <ButtonLink key={path} href={fileHref(repo, rev, path)} size="sm" variant="ghost" className="mono underline">{path}</ButtonLink>
      ))}</span> : null}
    </>
  );
}

export const INSTRUCTIONS_CHANGE = "Change repository instructions through a pull request.";

export function InstructionContent({ file, repo }: { file: Instruction; repo: string }) {
  // The server marks included text with HTML comments. The links above give the reader those paths.
  const source = file.content.replace(/^<!-- @[^\n]* -->\s*$/gm, "");
  if (!source.trim()) return null;
  // The instructions are nested under a section and a file heading.
  const html = renderMarkdown(source, repo, undefined, 3);
  return <div className="prose-bee [overflow-wrap:anywhere]" dangerouslySetInnerHTML={{ __html: html }} />;
}

const ROLE_TEXT: Record<string, string> = {
  REPO_ROLE_NONE: "has no repository role",
  REPO_ROLE_READ: "can read",
  REPO_ROLE_TRIAGE: "can read and triage issues",
  REPO_ROLE_WRITE: "can read, write and review pull requests",
  REPO_ROLE_MAINTAIN: "can read, write, review pull requests and manage settings",
  REPO_ROLE_ADMIN: "can read, write, review pull requests and administer this repository",
};

type Collaborator = Res<"RepoService", "ListCollaborators">["collaborators"][number];

export function collaboratorSentence({ principal, customRoleId, role }: Collaborator): string | null {
  if (!principal) return null;
  const name = principal.display || principal.id;
  const who = principal.kind === "PRINCIPAL_KIND_AGENT" ? name + " (agent)" : name;
  const permission = ROLE_TEXT[role];
  if (customRoleId) return who + " has custom role " + customRoleId + ".";
  return permission ? who + " " + permission + "." : null;
}

type TeamGrant = Res<"TeamService", "ListRepoTeams">["grants"][number];

export function teamSentence(grant: TeamGrant, teams: Team[]): string {
  const name = teams.find((team) => team.id === grant.teamId)?.name || grant.teamId;
  const via = grant.inherited && grant.viaTeamId ? " through " + (teams.find((team) => team.id === grant.viaTeamId)?.name || grant.viaTeamId) : "";
  const permission = ROLE_TEXT["REPO_ROLE_" + grant.role.toUpperCase()];
  if (permission) return "The " + name + " team " + permission + via + ".";
  return "The " + name + " team has custom role " + grant.role + via + ".";
}

type DeployKey = Res<"RepoService", "ListDeployKeys">["keys"][number];

export function keySentence(key: DeployKey): string {
  return (key.title || key.id) + (key.readOnly ? " can read this repository." : " can read and write this repository.");
}

const SUBJECTS: Record<string, string> = {
  "@everyone": "everyone",
  "@humans": "people",
  "@agents": "agents",
};

function object(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : {};
}

export function accessSentences(normalizedJson: string): string[] {
  if (!normalizedJson) return [];
  let access: unknown;
  try {
    access = object(JSON.parse(normalizedJson)).access;
  } catch {
    return [];
  }
  return Object.entries(object(access)).flatMap(([path, rule]) => {
    const scope = path === "/" ? "the repository" : path;
    return (["read", "write"] as const).flatMap((action) => {
      const subjects = object(rule)[action];
      if (!Array.isArray(subjects) || !subjects.every((subject) => typeof subject === "string")) return [];
      return [
        subjects.length
          ? "The rules restrict " + (action === "read" ? "reading" : "writing to") + " " + scope + " to " + listing(subjects.map((subject) => SUBJECTS[subject] ?? subject)) + "."
          : "The rules deny " + (action === "read" ? "reads of " : "writes to ") + scope + ".",
      ];
    });
  });
}

export function SettingRows({ items }: { items: ReactNode[] }) {
  return items.length ? (
    <ul className="divide-y divide-line">
      {items.map((item, index) => <li key={index} className="py-3 [overflow-wrap:anywhere]">{item}</li>)}
    </ul>
  ) : null;
}

export function moreLinks(repo: string, owner: string) {
  return [
    { label: "Organization settings", href: orgSettingsHref(owner), description: "The organization and its repositories." },
    { label: "People and teams", href: orgSettingsHref(owner, "members"), description: "Members, roles and teams in the organization." },
    { label: "Apps and integrations", href: orgSettingsHref(owner, "apps"), description: "Apps the organization owns and their installations." },
    { label: "Organization record", href: orgSettingsHref(owner, "audit"), description: "The audit log for changes to settings and permissions." },
    { label: "Branches", href: repoHref(repo, "code"), description: "Browse repository branches in Code." },
    { label: "CI", href: repoHref(repo, "runs"), description: "Workflow runs, checks and logs." },
    { label: "Security", href: repoHref(repo, "security"), description: "Dependency, secret and code scanning alerts." },
  ];
}
