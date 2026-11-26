/**
 * /llms.txt: the index an agent reads to learn what Beehive is and where everything is. Built
 * from lib/prompts.ts, so it can never disagree with /prompts. Format: an H1 with the name, a
 * blockquote summary, then H2 sections of links, and an "Optional" section with the full text.
 */
import { AGENTS_GUIDE_URL, DOCS_BASE, fillPrompt, PROMPT_GROUPS, PROMPTS, SELF_HOSTING_URL, SPEC_URL, type Prompt } from "./prompts";

export interface LlmsTargets {
  endpoint: string;
  origin: string;
}

const trim = (url: string) => url.replace(/\/+$/, "");
/** One line: the prompt text on a link line must not break the list. */
const oneLine = (s: string) => s.replace(/\s+/g, " ").trim();

function promptLinks(origin: string): string {
  const byId = new Map<string, Prompt>(PROMPTS.map((p) => [p.id, p]));
  const lines: string[] = [];
  for (const group of PROMPT_GROUPS) {
    for (const id of group.ids) {
      const p = byId.get(id);
      if (p) lines.push(`- [${p.title}](${origin}/prompts#${p.id}): ${oneLine(p.when)}`);
    }
  }
  return lines.join("\n");
}

export function buildLlmsTxt(targets: LlmsTargets): string {
  const endpoint = trim(targets.endpoint);
  const origin = trim(targets.origin);
  const optional = PROMPTS.map((p) => `### ${p.title}\n\n${fillPrompt(p.body, { endpoint })}`).join("\n\n");
  return `# Beehive

> Beehive hosts Bee, source control built for AI agents. History, merges, rebases and landing run on the server, and every operation is available three ways that behave identically: the \`bee\` CLI, an MCP server and an HTTP/JSON API. This instance's API is at ${endpoint}.

Start with the agents guide, then use the prompts for the job in front of you. People judge a change request from its brief and evidence (screenshots, video, benchmarks), not from the diff: the \"Show your work on a change request\" prompt says how to write them. Public repositories can be read without a token. Everything else needs a bearer token; send it as \`Authorization: Bearer <token>\`. Failed calls return a stable \`reason\`, a \`message\` and a \`retryable\` flag; retry only when \`retryable\` is true.

## Docs

- [Agents guide](${AGENTS_GUIDE_URL}): CLI, MCP and HTTP side by side, tokens and budgets, leases, change requests and landing, error reasons, the local daemon, anonymous read.
- [Self-hosting guide](${SELF_HOSTING_URL}): run your own Beehive with Docker Compose.
- [Specification](${SPEC_URL}): the contract: identifiers, tokens, policy, protocol and errors.
- [Agent prompts](${origin}/prompts): copy-paste prompts for common jobs, filled in for this instance.

## Prompts

${promptLinks(origin)}

## API

- [HTTP/JSON API](${endpoint}): \`POST ${endpoint}/bee.v1.<Service>/<Method>\` with a proto3-JSON body, for example \`${endpoint}/bee.v1.AuthService/WhoAmI\`.
- [MCP server](${endpoint}/mcp): streamable HTTP; every RPC is a tool named \`<service>_<method>\`, and \`search_tools\` finds any of them.
- [OpenAPI description](${DOCS_BASE}/schemas/openapi/bee.v1.json): every method and message of the HTTP/JSON surface.
- [CLI index](${DOCS_BASE}/docs/agents-guide.md#1-three-forms-of-every-operation): \`bee help-agent\` prints every command with its schema path.

## Optional

The full text of each prompt, so you do not need a second request.

${optional}
`;
}

const cache = new Map<string, string>();

/** buildLlmsTxt, remembered per endpoint and origin: the answer only changes with the deploy. */
export function llmsTxt(targets: LlmsTargets): string {
  const key = `${trim(targets.endpoint)}\n${trim(targets.origin)}`;
  let text = cache.get(key);
  if (text === undefined) {
    if (cache.size >= 16) cache.clear();
    text = buildLlmsTxt(targets);
    cache.set(key, text);
  }
  return text;
}
