/**
 * The redesigned screens rendered whole, against the demo world, in both modes: the page's own
 * server component is called with the fixture adapter behind it and its output is rendered to
 * HTML. It checks what a unit test of one function cannot: that each screen loads, shows the
 * facts it promises in the right words, and has one `h1`.
 */
import { createElement, type ReactNode } from "react";
import { prerender } from "react-dom/static";
import { beforeAll, describe, expect, it, vi } from "vitest";

const state = vi.hoisted(() => {
  process.env.BEE_ADAPTER = "fixture";
  process.env.BEE_TOKEN = "mara-token-0001";
  process.env.BEE_FIXTURE_LIVE = "0";
  // Keep running jobs present while the suite renders the CI screens.
  process.env.BEE_FIXTURE_LOG_MS = "600000";
  return { mode: "keeper", path: "/" };
});

vi.mock("next/headers", () => ({
  cookies: async () => ({ get: (name: string) => (name === "bee_mode" ? { value: state.mode } : undefined) }),
  headers: async () => new Headers(),
}));
vi.mock("next/navigation", () => ({
  useRouter: () => ({ refresh: () => {}, push: () => {}, replace: () => {} }),
  usePathname: () => state.path,
  useSearchParams: () => new URLSearchParams(),
  useSelectedLayoutSegment: () => null,
  redirect: (to: string) => {
    throw new Error("redirect " + to);
  },
  notFound: () => {
    throw new Error("not found");
  },
}));

async function render(node: ReactNode): Promise<string> {
  const { prelude } = await prerender(node);
  return await new Response(prelude as unknown as BodyInit).text();
}

/** The text a reader sees: tags, scripts and templates dropped, whitespace collapsed. */
function text(html: string): string {
  return html
    .replace(/<(script|template|style)[\s\S]*?<\/\1>/g, "")
    .replace(/<!--[\s\S]*?-->/g, "")
    .replace(/<[^>]+>/g, " ")
    .replace(/&amp;/g, "&")
    .replace(/&quot;/g, '"')
    .replace(/&#x27;/g, "'")
    .replace(/\s+/g, " ")
    .trim();
}

import { writeFileSync } from "node:fs";
const DUMP = process.env.BEE_SCREEN_DUMP;
const repo = { owner: "acme", repo: "api" };
const sp = (q: Record<string, string> = {}) => Promise.resolve(q);
const seen: Record<string, string> = {};

async function screen(name: string, mode: "keeper" | "bee", path: string, build: () => Promise<ReactNode>): Promise<{ html: string; text: string }> {
  state.mode = mode;
  state.path = path;
  const html = await render(await build());
  const out = { html, text: text(html) };
  seen[name + " (" + mode + ")"] = out.text;
  return out;
}

const count = (html: string, re: RegExp) => html.match(re)?.length ?? 0;
/** The agent's view: facts and tools, nothing to press. */
function expectAgentView(html: string, page: string) {
  expect(html).toContain('data-bee-page="' + page + '"');
  expect(html).toContain("## tools on this page");
  expect(count(html, /data-tool="/g)).toBeGreaterThan(0);
  expect(html).not.toContain("<button");
}
/** Bee's own terms, which a person must not meet outside a command shown as code. */
const OLD_WORDS = /\b(Inbox|Landed|landed|Ready to land|[Ll]and(s|ing)?\b(?! rules)|[Cc]hange requests?|[Cc]hangesets?)\b/;

// Each test renders whole pages, and the first one also pays for loading the app's modules.
describe("screens on the demo world", { timeout: 30_000 }, () => {
  beforeAll(() => () => {
    if (DUMP) writeFileSync(DUMP, Object.entries(seen).map(([k, v]) => "=== " + k + "\n" + v).join("\n\n"));
  });

  it("Needs you: one list of sentences, risk first, each with its repository, branch, rule and person", async () => {
    const { default: Home } = await import("@/app/(public)/page");
    const { html, text: t } = await screen("needs", "keeper", "/", async () => Home());
    expect(count(html, /<h1/g)).toBe(1);
    expect(t).toMatch(/Needs you · all repositories (Nothing needs|\w+ (pull requests? needs?|things? needs?)) you/);
    expect(t).toMatch(/acme\/api #101 · triage wants to merge Rotate session ids on role change \. A change to a path the rules mark sensitive needs two approvals from a person\. It has one\./);
    expect(t).toContain("Mara Okafor answers for it");
    expect(t).toContain("ws/triage/rotate-sessions");
    expect(t).toMatch(/commits? merged today across/);
    for (const part of ["In progress", "Merged today", "This week"]) expect(t).toContain(part);
    // The sensitive change is listed before the dependency bump.
    expect(t.indexOf("acme/api #101")).toBeLessThan(t.indexOf("acme/api #104"));
    // One solid button on the screen, and the mark only beside people who answer.
    expect(count(html, /bg-accent /g)).toBe(1);
    expect(count(html, /class="dot"/g)).toBe(count(html, /answers for it/g));
    expect(t).not.toMatch(OLD_WORDS);

    const bee = await screen("needs", "bee", "/", async () => Home());
    expectAgentView(bee.html, "needs");
    expect(bee.text).toContain("acme/api #101 · ws/triage/rotate-sessions → main · triage wants to merge “Rotate session ids on role change”.");
    expect(bee.text).toContain("change_evaluate_landing");
    expect(count(bee.html, /<h1/g)).toBe(1);
  });

  it("Code: the README first, then Files, then Commits with the agent and the person behind each", async () => {
    const { default: RepoHome } = await import("@/app/(repo)/[owner]/[repo]/page");
    const { html, text: t } = await screen("code", "keeper", "/acme/api", async () => RepoHome({ params: Promise.resolve(repo) }));
    const order = ["readme-heading", "files-heading", "commits-heading", "editing-heading", "about-heading"].map((id) => html.indexOf('id="' + id + '"'));
    expect(order.every((i) => i > 0)).toBe(true);
    expect([...order].sort((a, b) => a - b)).toEqual(order);
    expect(html).not.toContain("<aside");
    expect(t).toContain("Tighten token audience check");
    expect(t).toMatch(/docs-writer · for Lin Zhou/);
    // The language bar is ink and greys: no colour from the server reaches the page.
    expect(html).not.toMatch(/background:#[0-9a-f]{3,6}/i);
    expect(t).not.toMatch(OLD_WORDS);

    const bee = await screen("code", "bee", "/acme/api", async () => RepoHome({ params: Promise.resolve(repo) }));
    expectAgentView(bee.html, "code");
    expect(bee.text).toContain("default branch main");
    expect(bee.text).toContain("query_log");
    // The repository's name is the page's h1, in the header above.
    expect(bee.html).not.toContain("<h1");
  });

  it("Pull requests: grouped by who they wait on, each row with its state, agent, person and branch", async () => {
    const { default: Pulls } = await import("@/app/(repo)/[owner]/[repo]/changes/page");
    const { html, text: t } = await screen("pulls", "keeper", "/acme/api/changes", async () => Pulls({ params: Promise.resolve(repo), searchParams: sp() }));
    expect(count(html, /<h1/g)).toBe(1);
    expect(html).toMatch(/<h1[^>]*>Pull requests<\/h1>/);
    const groups = [...html.matchAll(/data-group="([^"]+)"/g)].map((m) => m[1]);
    expect(groups).toEqual(["needs-you", "checks", "waiting", "landed"]);
    for (const label of ["Needs you", "Checks running", "Waiting on someone else", "Merged this week"]) expect(t).toContain(label);
    expect(t).toMatch(/#101 Rotate session ids on role change Needs you · triage · for Mara Okafor · ws\/triage\/rotate-sessions/);
    expect(t).toMatch(/#105 Show the tax breakdown on invoice PDFs Checks running/);
    expect(t).toMatch(/Open \d+ Merged \d+ Closed \d+ All \d+/);
    expect(count(html, /bg-accent /g)).toBe(0);
    expect(t).not.toMatch(OLD_WORDS);

    const merged = await screen("pulls merged", "keeper", "/acme/api/changes", async () => Pulls({ params: Promise.resolve(repo), searchParams: sp({ state: "landed" }) }));
    expect(merged.text).toMatch(/#106 Add health route latency histogram Merged/);
    expect(merged.html).toMatch(/aria-current="page"[^>]*href="\/acme\/api\/changes\?state=merged"|href="\/acme\/api\/changes\?state=merged"[^>]*aria-current="page"/);

    const bee = await screen("pulls", "bee", "/acme/api/changes", async () => Pulls({ params: Promise.resolve(repo), searchParams: sp() }));
    expectAgentView(bee.html, "pulls");
    expect(bee.text).toMatch(/#101 · Rotate session ids on role change · .* · triage for Mara Okafor · ws\/triage\/rotate-sessions → main/);
    expect(count(bee.html, /<h1/g)).toBe(1);
  });

  it("A pull request: title, merge sentence, person, decisions, then why you, checks, files, commits, who did it", async () => {
    const { default: Layout } = await import("@/app/(repo)/[owner]/[repo]/changes/[id]/layout");
    const { default: Page } = await import("@/app/(repo)/[owner]/[repo]/changes/[id]/page");
    const { createClient } = await import("@/lib/api/client");
    const { fixtureRpc, getFixtureStore } = await import("@/lib/api/fixture");
    const api = createClient(fixtureRpc(getFixtureStore(), process.env.BEE_TOKEN));
    const { changes } = await api.change.listChanges({ repo: { fullName: "acme/api" } });
    const byNumber = (n: string) => changes.find((c) => String(c.number) === n)?.id ?? "";
    const params = (n: string) => Promise.resolve({ ...repo, id: byNumber(n) });
    const pull = (n: string) => async () => Layout({ params: params(n), children: await Page({ params: params(n), searchParams: sp() }) });

    const { html, text: t } = await screen("pull 101", "keeper", "/acme/api/changes/x", pull("101"));
    expect(count(html, /<h1/g)).toBe(1);
    expect(t).toContain("#101 Rotate session ids on role change");
    expect(t).toMatch(/triage wants to merge into main from ws\/triage\/rotate-sessions/);
    expect(t).toContain("Mara Okafor answers for this agent.");
    // The sections, in the plan's order.
    const order = ["pull-why", "pull-checks", "pull-files", "pull-commits", "pull-who"].map((id) => html.indexOf('id="' + id + '"'));
    expect(order.every((i) => i > 0)).toBe(true);
    expect([...order].sort((a, b) => a - b)).toEqual(order);
    expect(t).toContain("A change to a path the rules mark sensitive needs two approvals from a person. It has one.");
    expect(t).toContain("approval:human:sensitive (1 of 2)");
    expect(t).toContain("Checks 1 of 3 checks still running. Four pieces of evidence are attached.");
    // Who did it, with the prompt digest the API returns.
    expect(html).toContain('data-testid="prompt-digest"');
    expect(t).toContain("b8152e3ad0ed5346a175d0a9506e39bb");
    expect(t).toContain("claude-opus-5");
    expect(t).toContain("$0.87");
    // Mara asked for changes earlier and has not approved the latest revision: one solid button.
    expect(html).toContain(">Approve and merge<");
    expect(html).toContain(">Request changes<");
    expect(html).toContain(">Close<");
    expect(count(html, /bg-accent /g)).toBe(1);
    expect(t).not.toMatch(OLD_WORDS);

    // Her own pull request: she cannot approve it, and nothing is solid.
    const mine = await screen("pull 109", "keeper", "/acme/api/changes/x", pull("109"));
    expect(mine.html).not.toContain(">Approve and merge<");
    expect(mine.html).toContain(">Close<");
    expect(mine.text).toContain("opened this and answers for it.");
    // A person wrote it: no model, no cost, no prompt digest is made up.
    expect(mine.html).not.toContain('data-testid="prompt-digest"');

    // Merged: nothing to decide, and no rule section.
    const merged = await screen("pull 106", "keeper", "/acme/api/changes/x", pull("106"));
    expect(merged.text).toContain("It merged. Nothing needs you here.");
    expect(merged.html).not.toContain('id="pull-why"');
    expect(merged.text).toMatch(/triage merged into main/);

    const bee = await screen("pull 101", "bee", "/acme/api/changes/x", pull("101"));
    expectAgentView(bee.html, "pull");
    expect(count(bee.html, /<h1/g)).toBe(1);
    expect(bee.text).toContain("missing: approval:human:sensitive (1 of 2)");
    expect(bee.text).toContain("prompt digest b8152e3ad0ed5346a175d0a9506e39bb");
    expect(bee.html).toMatch(/data-tool="change_enqueue_change" data-kind="needs a person"/);
  });

  it("Issues: who is working on each, and for whom; an issue page with its pull requests", async () => {
    const { default: Issues } = await import("@/app/(repo)/[owner]/[repo]/work/page");
    const { default: Issue } = await import("@/app/(repo)/[owner]/[repo]/work/[number]/page");
    const { html, text: t } = await screen("issues", "keeper", "/acme/api/work", async () => Issues({ params: Promise.resolve(repo), searchParams: sp() }));
    expect(count(html, /<h1/g)).toBe(1);
    expect(html).toMatch(/<h1[^>]*>Issues<\/h1>/);
    expect(t).toMatch(/#15 Tax lines drift by a cent on multi-line invoices billing-fixer is working on it · for Dev Patel · pull request #102 \(open\)/);
    expect(t).toMatch(/#20 Cache invoice totals per customer Lin Zhou has it/);
    expect(t).toMatch(/#22 Paginate GET \/v1\/invoices Nobody has it yet/);
    expect(html).toContain(">Open issue<");
    expect(count(html, /bg-accent /g)).toBe(1);
    expect(t).not.toMatch(OLD_WORDS);

    const one = await screen("issue 15", "keeper", "/acme/api/work/15", async () => Issue({ params: Promise.resolve({ ...repo, number: "15" }) }));
    expect(count(one.html, /<h1/g)).toBe(1);
    expect(one.text).toContain("#15 Tax lines drift by a cent on multi-line invoices");
    expect(one.text).toMatch(/billing-fixer is working on it, for Dev Patel\./);
    expect(one.text).toMatch(/Pull requests #102 Fix rounding drift in invoice tax lines Open/);
    expect(one.text).not.toMatch(OLD_WORDS);

    const bee = await screen("issues", "bee", "/acme/api/work", async () => Issues({ params: Promise.resolve(repo), searchParams: sp() }));
    expectAgentView(bee.html, "issues");
    expect(bee.text).toContain("#15 · Tax lines drift by a cent on multi-line invoices · open · billing-fixer is working on it for Dev Patel");
    const beeOne = await screen("issue 15", "bee", "/acme/api/work/15", async () => Issue({ params: Promise.resolve({ ...repo, number: "15" }) }));
    expectAgentView(beeOne.html, "issue");
    expect(beeOne.html).toContain('data-tool="issue_close_issue"');
    expect(beeOne.html).not.toContain('data-tool="issue_reopen_issue"');
    expect(count(beeOne.html, /<h1/g)).toBe(1);
  });

  it("Feed, post, compose and updates share the fixture facts in both modes", async () => {
    const { default: Feed } = await import("@/app/(public)/feed/page");
    const { default: Updates } = await import("@/app/(repo)/[owner]/[repo]/updates/page");
    const { default: Posts } = await import("@/app/(repo)/[owner]/[repo]/posts/page");
    const { default: Post } = await import("@/app/(repo)/[owner]/[repo]/posts/[postId]/page");
    const { default: Compose } = await import("@/app/(repo)/[owner]/[repo]/posts/new/page");
    const { default: Choose } = await import("@/app/(public)/feed/new/page");
    const { repoMore, isActive } = await import("./nav");
    const updatesItem = repoMore().find((item) => item.id === "updates")!;
    expect(isActive(updatesItem, "/lattice/engine/posts/new", "lattice/engine")).toBe(true);
    expect(isActive(updatesItem, "/lattice/engine/posts", "lattice/engine")).toBe(true);
    expect(isActive(updatesItem, "/lattice/engine/work", "lattice/engine")).toBe(false);
    const { createClient } = await import("@/lib/api/client");
    const { fixtureRpc, getFixtureStore } = await import("@/lib/api/fixture");
    const store = getFixtureStore();
    const api = createClient(fixtureRpc(store, process.env.BEE_TOKEN));
    const engine = { owner: "lattice", repo: "engine" };
    const { posts } = await api.post.listRepoPosts({ repo: { fullName: "lattice/engine" } });
    const rich = posts.find((p) => p.bodyMarkdown && p.media.length && p.attachments?.length === 2);
    const article = posts.find((p) => p.article);
    expect(rich).toBeDefined();
    expect(article).toBeDefined();
    const richId = rich!.id;
    const articleId = article!.id;
    const testRepo = { fullName: "acme/api" };
    const token = await api.auth.mintToken({
      principalId: store.p("triage").id, scope: { repos: ["acme/api"], actions: ["read", "commit"], readPaths: [], writePaths: [], refs: [], session: "post-screen-test", expiresAt: new Date(store.now() + 60_000).toISOString() },
    });
    const triage = createClient(fixtureRpc(store, token.token));
    const pending = await triage.post.createPost({ repo: testRepo, text: "A summary for the approval screen" });

    function assertOrder(html: string, ...parts: string[]) {
      const order = parts.map((part) => html.indexOf(part));
      expect(order.every((n) => n >= 0), parts.join(", ")).toBe(true);
      expect(order).toEqual([...order].sort((a, b) => a - b));
    }

    for (const mode of ["keeper", "bee"] as const) {
      const feed = await screen("feed", mode, "/feed", () => Feed({ searchParams: sp() }));
      const all = await screen("everything", mode, "/feed?tab=everything", () => Feed({ searchParams: sp({ tab: "everything" }) }));
      const updates = await screen("updates", mode, "/lattice/engine/updates", () => Updates({ params: Promise.resolve(engine), searchParams: sp() }));
      const alias = await screen("posts", mode, "/lattice/engine/posts", () => Posts({ params: Promise.resolve(engine), searchParams: sp() }));
      const post = await screen("post", mode, "/lattice/engine/posts/x", () => Post({ params: Promise.resolve({ ...engine, postId: richId }) }));
      const long = await screen("article", mode, "/lattice/engine/posts/y", () => Post({ params: Promise.resolve({ ...engine, postId: articleId }) }));
      const approve = await screen("pending post", mode, "/acme/api/posts/z", () => Post({ params: Promise.resolve({ ...repo, postId: pending.id }) }));
      const compose = await screen("compose", mode, "/acme/api/posts/new", () => Compose({ params: Promise.resolve(repo) }));
      const pick = await screen("choose repository", mode, "/feed/new", () => Choose({ searchParams: sp() }));

      for (const page of [feed, all, updates, alias, post, long, approve, compose, pick]) {
        expect(count(page.html, /<h1[ >]/g)).toBe(1);
        // The vocabulary belongs to the interface. The fixture's authored body and image
        // descriptions quote historical screen names, which the application preserves.
        const ui = text(page.html
          .replace(/<(a|span) [^>]*data-post-content[^>]*>[\s\S]*?<\/\1>/g, "")
          .replace(/<div data-post-body="[^"]*"><div class="prose-bee [^"]*">[\s\S]*?<\/div>/g, ""));
        expect(ui).not.toMatch(OLD_WORDS);
        expect(ui).not.toMatch(/\b(?:Trellis|TrellisHub|Nucleus|Terramind)\b/);
      }
      expect(all.text).toContain("Order is not an input");
      expect(post.text).toContain("for Rhea");
      expect(long.text).toContain("How it goes wrong");
      expect(approve.text).toContain("Waiting for Mara Okafor to approve");
      expect(compose.text).toContain("Rotate session ids on role change");

      if (mode === "keeper") {
        expect(feed.html).toMatch(/aria-current="page"[^>]*href="\/feed"|href="\/feed"[^>]*aria-current="page"/);
        expect(feed.text).toContain("Write a post");
        const richCard = updates.html.split('data-testid="post-card"').find((part) => part.includes('data-post-id="' + richId + '"'));
        expect(richCard).toBeDefined();
        assertOrder(richCard!, "patcher", 'data-testid="post-summary"', 'data-testid="post-media"', 'data-testid="post-attachment-row"', 'data-testid="post-actions"');
        expect(richCard).toContain("2 pull requests");
        expect(richCard).toContain('aria-expanded="false"');
        expect(richCard).toContain(">More<");
        expect(count(updates.html, /bg-accent /g)).toBe(1);
        expect(post.html).toContain('data-state="merged"');
        expect(post.html).toContain("<strong>Three-way rebase</strong>");
        expect(long.html).toContain('data-testid="post-article"');
        expect(approve.html).toContain('data-agent-confirm="Approves and publishes this post"');
        assertOrder(compose.html, "Post text", "Body Markdown", "Article title", "Attach from this repository", "Add images or a video", "Publish post");
      } else {
        expectAgentView(feed.html, "feed");
        expectAgentView(all.html, "feed");
        expectAgentView(updates.html, "updates");
        expectAgentView(alias.html, "updates");
        expectAgentView(post.html, "post");
        expectAgentView(compose.html, "compose");
        expectAgentView(pick.html, "compose");
        assertOrder(post.html, ">author<", ">summary<", ">media<", ">attachments<", ">body Markdown<", "## tools on this page");
        expect(approve.html).toContain('data-tool="post_approve_post" data-kind="needs a person"');
      }
    }
  });

  it("Issues: the composer comes first and the detail puts the request before linked work and comments", async () => {
    const { default: Issues } = await import("@/app/(repo)/[owner]/[repo]/work/page");
    const { default: Issue } = await import("@/app/(repo)/[owner]/[repo]/work/[number]/page");
    const before = (html: string, ...parts: string[]) => {
      let previous = -1;
      for (const part of parts) {
        const index = html.indexOf(part);
        expect(index, part).toBeGreaterThan(previous);
        previous = index;
      }
    };
    const list = await screen("new issue", "keeper", "/acme/api/work", () => Issues({ params: Promise.resolve(repo), searchParams: sp() }));
    before(list.html, 'data-testid="new-issue"', ">More detail<", ">Open issue<", 'aria-label="Issue state"', "work/board");
    const agentList = await screen("open issue tool", "bee", "/acme/api/work", () => Issues({ params: Promise.resolve(repo), searchParams: sp() }));
    expectAgentView(agentList.html, "issues");
    expect(count(agentList.html, /<h1/g)).toBe(1);
    before(agentList.html, 'data-bee-group="facts"', 'data-bee-group="open issues"', 'data-tool="issue_create_issue"');
    expect(agentList.text).not.toMatch(OLD_WORDS);

    const params = Promise.resolve({ ...repo, number: "12" });
    const person = await screen("issue conversation", "keeper", "/acme/api/work/12", () => Issue({ params }));
    before(person.html, "<h1", "asked by", 'data-testid="issue-work"', "Demoting an admin", 'id="subs-title"', 'id="linked-title"', 'id="assign-title"', 'id="conv-title"', "Lease acquired");
    expect(person.html).toContain('data-agent-confirm="Closes issue #12"');
    expect(person.html).toContain('data-state="open"');
    expect(person.html).toContain('class="dot"');
    expect(count(person.html, /bg-accent /g)).toBe(1);
    expect(count(person.html, /<h1/g)).toBe(1);
    expect(person.text).not.toMatch(OLD_WORDS);

    const agent = await screen("issue conversation", "bee", "/acme/api/work/12", () => Issue({ params }));
    expectAgentView(agent.html, "issue");
    before(agent.html, "<h1", 'data-bee-group="facts"', 'data-bee-group="request"', "Demoting an admin", 'data-bee-group="sub-issues', 'data-bee-group="pull requests"', 'data-bee-group="assigned to"', 'data-bee-group="conversation"', "Lease acquired", 'data-bee-group="tools"');
    expect(agent.html).toContain('data-tool="issue_close_issue" data-kind="needs a person"');
    expect(count(agent.html, /<h1/g)).toBe(1);
    expect(agent.text).not.toMatch(OLD_WORDS);
  });

  it("Issue board: project columns and cards are in server order in both modes", async () => {
    const { default: Board } = await import("@/app/(repo)/[owner]/[repo]/work/board/page");
    for (const mode of ["keeper", "bee"] as const) {
      const board = await screen("issue board", mode, "/acme/api/work/board", () => Board({ params: Promise.resolve(repo) }));
      expect(count(board.html, /<h1/g)).toBe(1);
      expect(board.text).toMatch(/API · October .*Todo .*Write an audit entry.*In progress .*Rotate the session id.*In review .*Tax lines drift.*Done .*Health route/);
      expect(board.text).not.toMatch(OLD_WORDS);
      if (mode === "keeper") {
        expect(board.html).toContain('aria-label="Move #14 to"');
        expect(board.html).toContain('data-state="closed"');
      } else {
        expectAgentView(board.html, "issue-board");
        expect(board.html).toContain('data-tool="project_set_project_item_field_value"');
      }
    }
  });
});

describe("repository and discovery screens", { timeout: 30_000 }, () => {
  it("shows the same visible repositories and editing activity in both modes", async () => {
    const { default: Repositories } = await import("@/app/(public)/repositories/page");
    const build = () => Repositories({ searchParams: sp({ tab: "orgs", q: "api" }) });
    const keeper = await screen("repositories", "keeper", "/repositories?tab=orgs&q=api", build);
    expect(count(keeper.html, /<h1/g)).toBe(1);
    expect(keeper.html).toMatch(/data-repository="acme\/api"/);
    expect(keeper.html).toContain('href="/import"');
    expect(count(keeper.html, /bg-accent /g)).toBe(1);
    expect(keeper.text).toMatch(/\d+ agents? editing now/);
    expect(keeper.text).toMatch(/[1-9]\d* pull requests? needs? you/);

    const bee = await screen("repositories", "bee", "/repositories?tab=orgs&q=api", build);
    expectAgentView(bee.html, "repositories");
    expect(bee.text).toMatch(/acme\/api · (?:Public|Private|Internal) · .*\d+ agents? editing now/);
    expect(bee.text).toContain("lease_list_leases");
    expect(bee.text).toMatch(/[1-9]\d* pull requests? needs? you/);
  });

  it("renders the fork relationship, marks sync for confirmation and identifies its agent tool", async () => {
    const { RepoHeader, RepoHeaderFacts } = await import("@/components/repo-header");
    const props = {
      fullName: "mara/api", description: "API fork", visibility: "VISIBILITY_PRIVATE" as const, canWrite: true, canSync: true, signedIn: true,
      star: { starred: false, count: 3 }, forks: 2, counts: { changes: 1, issues: 0 }, upstream: { fullName: "acme/api", behind: true },
    };
    const build = async () => createElement(RepoHeader, props);
    const keeper = await screen("repository header", "keeper", "/mara/api", build);
    expect(keeper.text).toMatch(/Star mara\/api · 3/);
    expect(keeper.text).toMatch(/Fork · 2/);
    expect(keeper.text).toMatch(/Forked from acme\/api Sync fork/);
    expect(keeper.html).toContain('data-agent-confirm="Updates the default branch of mara/api from acme/api"');
    expect(keeper.html).toContain('href="/acme/api"');

    const bee = await screen("repository header", "bee", "/mara/api", async () => createElement(RepoHeaderFacts, { ...props, following: true, followers: 4 }));
    expectAgentView(bee.html, "repository-header");
    expect(bee.text).toContain("forked from acme/api");
    expect(bee.text).toContain("followed by you yes");
    expect(bee.html).toMatch(/data-tool="repo_sync_fork" data-kind="needs a person"/);
    expect(bee.html).toMatch(/data-tool="repo_fork_repo" data-kind="needs a person"/);
  });

  it("loads the star action and count from the same fixture data as the write", async () => {
    const { repoStarState } = await import("@/lib/api/server");
    const { createClient } = await import("@/lib/api/client");
    const { fixtureRpc, getFixtureStore } = await import("@/lib/api/fixture");
    const api = createClient(fixtureRpc(getFixtureStore(), process.env.BEE_TOKEN));
    const fullName = "acme/web";
    const before = await repoStarState(fullName, true);
    expect(before).toMatchObject({ starred: expect.any(Boolean), count: expect.any(Number) });
    expect(before.unavailable).toBeUndefined();
    const request = { repo: { fullName } };
    try {
      if (before.starred) await api.user.unstarRepo(request);
      else await api.user.starRepo(request);
      const after = await repoStarState(fullName, true);
      expect(after).toMatchObject({ starred: !before.starred, count: Number(before.count) + (before.starred ? -1 : 1) });
    } finally {
      if (before.starred) await api.user.starRepo(request);
      else await api.user.unstarRepo(request);
    }
  });

  it("renders each discovery page in both modes with a single title", async () => {
    const { default: Explore } = await import("@/app/(public)/explore/page");
    const { default: Trending } = await import("@/app/(public)/trending/page");
    const { default: Leaderboards } = await import("@/app/(public)/leaderboards/page");
    for (const item of [
      { id: "explore", title: "Explore", build: Explore, tool: "search_list_featured_topics" },
      { id: "trending", title: "Trending", build: () => Trending({ searchParams: sp() }), tool: "search_list_trending_repos" },
      { id: "leaderboards", title: "Leaderboards", build: () => Leaderboards({ searchParams: sp() }), tool: "search_list_leaderboard" },
    ]) {
      const keeper = await screen(item.id, "keeper", "/" + item.id, item.build);
      expect(keeper.text).toContain(item.title);
      expect(count(keeper.html, /<h1/g)).toBe(1);
      const bee = await screen(item.id, "bee", "/" + item.id, item.build);
      expectAgentView(bee.html, item.id);
      expect(bee.html).toContain('data-tool="' + item.tool + '"');
      expect(count(bee.html, /<h1/g)).toBe(1);
    }
  });

  it("shows search results and creation workflows through the fixture in both modes", async () => {
    const { default: Search } = await import("@/app/(shell)/search/page");
    const { default: Import } = await import("@/app/(shell)/import/page");
    const { default: New } = await import("@/app/(public)/new/page");
    const { default: SignIn } = await import("@/app/signin/page");
    for (const item of [
      { id: "search", path: "/search", title: "Search", build: () => Search({ searchParams: sp({ q: "billing repo:acme/api path:README.md" }) }), tool: "search_search_code", fact: "acme/api · README.md" },
      { id: "import", path: "/import", title: "Import from GitHub", build: Import, tool: "import_start_import", fact: "previous imports" },
      { id: "new-repository", path: "/new", title: "New repository", build: New, tool: "repo_create_repo", fact: "owner and repository name" },
      { id: "sign-in", path: "/signin", title: "Create your account, or sign in", build: SignIn, tool: "auth_login", fact: "Continue with GitHub" },
    ]) {
      const keeper = await screen(item.id, "keeper", item.path, item.build);
      expect(keeper.text).toContain(item.title);
      expect(count(keeper.html, /<h1/g)).toBe(1);
      const bee = await screen(item.id, "bee", item.path, item.build);
      expect(bee.html).toContain('data-bee-page="' + item.id + '"');
      expect(bee.html).toContain('data-tool="' + item.tool + '"');
      expect(bee.text).toContain(item.fact);
      expect(count(bee.html, /<h1/g)).toBe(1);
    }
  });
});

describe("CI screens on the demo world", { timeout: 30_000 }, () => {
  const legacyWords = /\b(?:Trellis(?:Hub)?|Nucleus|Terramind|[Cc]hange requests?|[Cc]hangesets?|Landed)\b/;
  const at = (id: string) => Promise.resolve({ ...repo, id });
  const prefix = "/acme/api/runs";
  async function runId(status: "failed" | "waiting") {
    const { serverClient } = await import("./api/server");
    const api = await serverClient();
    const { runs } = await api.workflow.listRuns({ repo: { fullName: "acme/api" }, page: { pageSize: 100 } });
    const run = runs.find((r) => r.status === status && (status !== "failed" || r.ref.includes("tax-rounding")));
    if (!run) throw new Error("Missing fixture run " + status);
    return run.id;
  }

  for (const mode of ["keeper", "bee"] as const) {
    it(mode + " CI groups the run history and shows the recorded deployment", async () => {
      const { default: CI } = await import("@/app/(repo)/[owner]/[repo]/runs/page");
      const { html, text: t } = await screen("ci", mode, prefix, () => CI({ params: Promise.resolve(repo), searchParams: sp() }));
      expect(count(html, /<h1(?:\s|>)/g)).toBe(1);
      const group = (label: string) => html.indexOf((mode === "bee" ? 'data-bee-group="' : 'aria-label="') + label + '"');
      const order = ["Failed", "Waiting on a person", "Running"].map(group);
      order.push(html.indexOf("runs passed this week"), mode === "bee" ? group("Deployments") : html.indexOf('aria-labelledby="ci-deployments"'));
      expect(order.every((pos) => pos >= 0)).toBe(true);
      expect(order).toEqual([...order].sort((a, b) => a - b));
      expect(t).toMatch(/#\d+ Fix rounding drift in invoice tax lines/);
      expect(t).toContain("test failed");
      expect(t).toContain("Manual run");
      expect(t).toContain("staging");
      expect(t).toContain("Succeeded");
      expect(html).toContain('href="/acme/api/runs/workflows"');
      expect(t).not.toMatch(legacyWords);
      if (mode === "bee") {
        expectAgentView(html, "ci");
        expect(html).toContain('data-tool="deployment_list_deployments"');
        expect(html).toMatch(/data-bee-group="\d+ runs passed this week"><details><summary/);
      } else {
        expect(html).toMatch(/<details class="border-t[^>]*><summary[^>]*>\d+ runs passed this week<\/summary>/);
        for (const shape of ["failed", "waiting", "running", "passed"]) expect(html).toContain('data-state="' + shape + '"');
        expect(html).not.toContain("Download badge");
      }
    });

    it(mode + " run opens with the summary, failed job and agent accountability", async () => {
      const { default: Run } = await import("@/app/(repo)/[owner]/[repo]/runs/[id]/page");
      const id = await runId("failed");
      const { html, text: t } = await screen("run", mode, prefix + "/" + id, () => Run({ params: at(id), searchParams: sp() }));
      expect(count(html, /<h1(?:\s|>)/g)).toBe(1);
      expect(t).toMatch(/Failed after [\dhms ]+: 1 of 3 jobs failed \(test\)/);
      expect(t.indexOf("What happened")).toBeLessThan(t.indexOf("Jobs"));
      expect(t).toContain("tests/billing.test.ts:17");
      expect(t).toContain("Fix rounding drift in invoice tax lines");
      expect(t).toContain("billing-fixer");
      expect(t).toMatch(/Answers for the agent \w/);
      expect(html).toContain("data-mark");
      expect(html).toContain("code?rev=");
      expect(t).not.toMatch(legacyWords);
      if (mode === "bee") {
        expectAgentView(html, "run");
        expect(html).toContain('data-tool="workflow_cancel_workflow_run" data-kind="needs a person"');
        expect(html).toContain('data-tool="workflow_tail_workflow_job_logs"');
      } else {
        expect(t).toContain("Annotations");
        expect(count(html, /<details open=""/g)).toBe(1);
        expect(count(html, /<details(?:\s|>)/g)).toBe(3);
        expect(count(html, /data-testid="log"/g)).toBe(1);
        expect(t).toContain("Re-run failed jobs");
        expect(t).toContain("Re-run all");
      }
    });

    it(mode + " workflow definitions keep controls separate from the CI list", async () => {
      const { default: Workflows } = await import("@/app/(repo)/[owner]/[repo]/runs/workflows/page");
      const { html, text: t } = await screen("workflows", mode, prefix + "/workflows", () => Workflows({ params: Promise.resolve(repo), searchParams: sp() }));
      expect(count(html, /<h1(?:\s|>)/g)).toBe(1);
      expect(t).toContain(".github/workflows/ci.yml");
      expect(t).toContain("Enabled");
      expect(t).not.toMatch(legacyWords);
      if (mode === "bee") {
        expectAgentView(html, "workflows");
        expect(html).toContain('data-tool="workflow_disable_workflow" data-kind="needs a person"');
      } else {
        expect(t).toContain("Download badge");
        expect(html).toContain('data-agent-confirm="Disables this workflow');
      }
    });
  }

  it("a held run offers confirmed cancellation without claiming that a job started", async () => {
    const { default: Run } = await import("@/app/(repo)/[owner]/[repo]/runs/[id]/page");
    const id = await runId("waiting");
    const { html, text: t } = await screen("waiting", "keeper", prefix + "/" + id, () => Run({ params: at(id), searchParams: sp() }));
    expect(t).toContain("Waiting on a person.");
    expect(t).toContain("No jobs have been reported for this run.");
    expect(t).not.toContain("Re-run all");
    expect(html).toContain('data-agent-confirm="Cancels this run and stops its unfinished jobs."');
  });
});
