/**
 * The agent prompts. The text lives in docs/prompts/*.md (the source of truth) and reaches the
 * app through lib/prompts.gen.ts, which scripts/prompts.mjs writes and `gen:check` verifies. This
 * module orders and groups them and fills the placeholders. It is free of server-only imports:
 * the prompt card (a client component) uses it too.
 */
import { GENERATED_PROMPTS } from "./prompts.gen";

export interface Prompt {
  id: string;
  title: string;
  /** One line on when to hand this prompt to an agent. */
  when: string;
  /** The prompt, in second person, with {{endpoint}}, {{repo}} and {{org}} placeholders. */
  body: string;
}

export interface PromptGroup {
  title: string;
  blurb: string;
  ids: readonly string[];
}

/** How /prompts groups the prompts, in display order. Every prompt is in exactly one group. */
export const PROMPT_GROUPS: readonly PromptGroup[] = [
  { title: "Get started", blurb: "Connect an agent to Beehive, or bring a repository over.", ids: ["setup-repo", "migrate-from-github"] },
  { title: "Daily work", blurb: "What an agent does in a repository every day.", ids: ["hold-lease", "open-and-land-change", "show-your-work", "write-a-post", "review-change", "triage-ci", "follow-and-adopt"] },
  { title: "Operate", blurb: "Run an instance and hand out access.", ids: ["mint-agent-token", "self-host-docker"] },
];

const byId = new Map(GENERATED_PROMPTS.map((p) => [p.id, p]));

/** All prompts in group order (the order /prompts and /llms.txt list them). */
export const PROMPTS: Prompt[] = PROMPT_GROUPS.flatMap((g) => g.ids.map((id) => byId.get(id))).filter((p): p is Prompt => !!p);

export interface PromptVars {
  /** The server's base URL, as agents reach it: no trailing slash needed. */
  endpoint: string;
  /** `owner/name`. Without it the prompt says `your-org/your-repo`. */
  repo?: string;
  /** Org name. Without it, the owner of `repo`, else `your-org`. */
  org?: string;
}

/** What `endpoint` falls back to when the caller has none: the address of `bee-server --dev`. */
export const DEFAULT_ENDPOINT = "http://127.0.0.1:7420";

/** Where the guides live in the source repository (the web app serves no docs of its own). */
export const DOCS_BASE = "https://github.com/terramind-org/bee/blob/main";

export const AGENTS_GUIDE_URL = DOCS_BASE + "/docs/agents-guide.md";
export const SELF_HOSTING_URL = DOCS_BASE + "/docs/self-hosting.md";
export const SPEC_URL = DOCS_BASE + "/SPEC.md";

function orElse(value: string | undefined, fallback: string): string {
  const v = (value ?? "").trim();
  return v || fallback;
}

/** Fills {{endpoint}}, {{repo}} and {{org}} in `body`. No placeholder survives. */
export function fillPrompt(body: string, vars: PromptVars): string {
  const endpoint = orElse(vars.endpoint, DEFAULT_ENDPOINT).replace(/\/+$/, "");
  const repo = orElse(vars.repo, "your-org/your-repo");
  const org = orElse(vars.org, vars.repo?.includes("/") ? repo.split("/")[0]! : "your-org");
  const values: Record<string, string> = { endpoint, repo, org };
  return body.replace(/\{\{(endpoint|repo|org)\}\}/g, (_, name: string) => values[name]!);
}

export function getPrompt(id: string): Prompt | undefined {
  return byId.get(id);
}

/** The prompt `id` with its placeholders filled. Throws for an id that does not exist. */
export function promptFor(id: string, vars: PromptVars): string {
  const prompt = byId.get(id);
  if (!prompt) throw new Error(`unknown prompt "${id}"`);
  return fillPrompt(prompt.body, vars);
}
