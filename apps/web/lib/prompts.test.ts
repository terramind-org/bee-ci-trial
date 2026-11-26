import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { DEFAULT_ENDPOINT, fillPrompt, getPrompt, PROMPT_GROUPS, PROMPTS, promptFor } from "./prompts";
import { GENERATED_PROMPTS } from "./prompts.gen";

const docs = join(__dirname, "..", "..", "..", "docs", "prompts");
const ids = ["setup-repo", "migrate-from-github", "open-and-land-change", "show-your-work", "write-a-post", "follow-and-adopt", "review-change", "hold-lease", "triage-ci", "self-host-docker", "mint-agent-token"];

describe("prompts", () => {
  it("ships every required prompt, each with a title, a when line and a body", () => {
    expect(PROMPTS.map((p) => p.id).sort()).toEqual([...ids].sort());
    for (const p of PROMPTS) {
      expect(p.title.length).toBeGreaterThan(3);
      expect(p.when.length).toBeGreaterThan(10);
      expect(p.body.length).toBeGreaterThan(400);
    }
  });

  it("puts every prompt in exactly one group", () => {
    const grouped = PROMPT_GROUPS.flatMap((g) => g.ids);
    expect(new Set(grouped).size).toBe(grouped.length);
    expect(grouped.sort()).toEqual(GENERATED_PROMPTS.map((p) => p.id).sort());
  });

  it("matches the files in docs/prompts one to one", () => {
    const files = readdirSync(docs).filter((n) => n.endsWith(".md"));
    expect(files.map((f) => f.replace(/\.md$/, "")).sort()).toEqual(GENERATED_PROMPTS.map((p) => p.id).sort());
    for (const f of files) {
      const text = readFileSync(join(docs, f), "utf8");
      const p = getPrompt(f.replace(/\.md$/, ""))!;
      expect(text).toContain("title: " + p.title);
      expect(text).toContain(p.body.slice(0, 60));
    }
  });

  it("uses only the three placeholders", () => {
    for (const p of PROMPTS) {
      for (const [, name] of p.body.matchAll(/\{\{([^{}]*)\}\}/g)) expect(["endpoint", "repo", "org"]).toContain(name);
    }
  });

  it("fills every placeholder", () => {
    for (const p of PROMPTS) {
      const text = promptFor(p.id, { endpoint: "https://bee.example.com/", repo: "acme/api", org: "acme" });
      expect(text).not.toContain("{{");
      expect(text).not.toContain("https://bee.example.com//");
    }
    const setup = promptFor("setup-repo", { endpoint: "http://127.0.0.1:7420", repo: "acme/api" });
    expect(setup).toContain("http://127.0.0.1:7420/mcp");
    expect(setup).toContain("bee git setup --repo acme/api");
  });

  it("fills missing optional values with words, not placeholders", () => {
    const text = promptFor("open-and-land-change", { endpoint: "http://x:1" });
    expect(text).toContain("your-org/your-repo");
    expect(text).not.toContain("{{");
    // The org defaults to the repo's owner.
    expect(fillPrompt("{{org}}/{{repo}}", { endpoint: "http://x:1", repo: "acme/api" })).toBe("acme/acme/api");
    expect(fillPrompt("{{org}}", { endpoint: "http://x:1" })).toBe("your-org");
    expect(fillPrompt("{{endpoint}}", { endpoint: "  " })).toBe(DEFAULT_ENDPOINT);
  });

  it("throws for an unknown id", () => {
    expect(() => promptFor("nope", { endpoint: "http://x:1" })).toThrow(/unknown prompt/);
  });

  it("links show-your-work from the prompts a change request passes through", () => {
    const show = getPrompt("show-your-work")!;
    for (const id of ["open-and-land-change", "review-change", "triage-ci"]) {
      expect(getPrompt(id)!.body, id).toContain(show.title);
    }
    // The commands it teaches are the ones the CLI has.
    for (const command of ["bee cr checklist", "bee cr brief", "bee cr evidence add", "bee cr evidence list", "bee cr evidence rm"]) {
      expect(show.body).toContain(command);
    }
    expect(PROMPT_GROUPS.find((g) => g.ids.includes("show-your-work"))?.title).toBe("Daily work");
  });

  it("never names a competitor or a broken or retired product name, and cites no placeholder command", () => {
    for (const p of PROMPTS) {
      expect(p.body).not.toMatch(/Trellis|Bee ?Hub|Bee Hive|Bee Bee|\ban Bee\b/i);
      expect(p.body).not.toMatch(/gitlab|bitbucket|copilot|codex|claude|\bcursor\b/i);
      expect(p.body).not.toMatch(/\bTODO\b|\bTBD\b|lorem/i);
    }
  });
});
