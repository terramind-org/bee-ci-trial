import { describe, expect, it } from "vitest";
import { buildLlmsTxt, llmsTxt } from "./llms";
import { PROMPT_GROUPS, PROMPTS } from "./prompts";

const targets = { endpoint: "https://api.bee.example/", origin: "https://bee.example" };

describe("llms.txt", () => {
  const text = buildLlmsTxt(targets);

  it("follows the llms.txt shape: H1, blockquote, then H2 sections", () => {
    const lines = text.split("\n");
    expect(lines[0]).toBe("# Beehive");
    expect(lines[2]).toMatch(/^> Beehive hosts Bee, source control built for AI agents/);
    expect(text.match(/^# /gm)).toHaveLength(1);
    const sections = [...text.matchAll(/^## (.+)$/gm)].map((m) => m[1]);
    expect(sections).toEqual(["Docs", "Prompts", "API", "Optional"]);
  });

  it("names the API and MCP URLs of this instance", () => {
    expect(text).toContain("https://api.bee.example/bee.v1.AuthService/WhoAmI");
    expect(text).toContain("[MCP server](https://api.bee.example/mcp)");
    expect(text).not.toContain("example//");
  });

  it("links every prompt once and includes every prompt in full under Optional", () => {
    const prompts = text.slice(text.indexOf("## Prompts"), text.indexOf("## API"));
    const optional = text.slice(text.indexOf("## Optional"));
    for (const p of PROMPTS) {
      expect(prompts).toContain(`[${p.title}](https://bee.example/prompts#${p.id})`);
      expect(optional).toContain(`### ${p.title}`);
    }
    expect(PROMPT_GROUPS.flatMap((g) => g.ids)).toHaveLength(PROMPTS.length);
    expect(optional).toContain("https://api.bee.example/mcp");
  });

  it("tells agents to show their work, and links the prompt that says how", () => {
    expect(text).toContain("brief and evidence");
    expect(text).toContain("[Show your work on a change request](https://bee.example/prompts#show-your-work)");
    expect(text).toContain("### Show your work on a change request");
  });

  it("leaves no placeholder and no broken or retired product name", () => {
    expect(text).not.toContain("{{");
    expect(text).not.toMatch(/Trellis|Bee ?Hub|Bee Hive|Bee Bee|\ban Bee\b/i);
  });

  it("is built once per endpoint and origin", () => {
    expect(llmsTxt(targets)).toBe(llmsTxt({ ...targets }));
    expect(llmsTxt(targets)).toBe(text);
    expect(llmsTxt({ endpoint: "http://127.0.0.1:7420", origin: "http://localhost:3100" })).toContain("http://127.0.0.1:7420/mcp");
  });
});
