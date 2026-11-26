import { prerender } from "react-dom/static";
import type { ReactNode } from "react";
import { describe, expect, it, vi } from "vitest";

const state = vi.hoisted(() => {
  process.env.BEE_ADAPTER = "fixture";
  process.env.BEE_TOKEN = "demo-token-1";
  process.env.BEE_FIXTURE_LIVE = "0";
  return { mode: "keeper" };
});

vi.mock("next/headers", () => ({
  cookies: async () => ({ get: (name: string) => name === "bee_mode" ? { value: state.mode } : undefined }),
  headers: async () => new Headers(),
}));
vi.mock("next/navigation", () => ({
  useRouter: () => ({ refresh: () => {} }),
  usePathname: () => "/acme/api/safety",
  useSearchParams: () => new URLSearchParams(),
  notFound: () => { throw new Error("Not found"); },
}));

import RepoSafety from "@/app/(repo)/[owner]/[repo]/safety/page";
import OrgSafety from "@/app/(shell)/orgs/[org]/settings/safety/page";
import { getFixtureStore } from "@/lib/api/fixture";
import { serverClient } from "@/lib/api/server";
import { pullSafetyLink } from "@/components/safety/pull-record";
import { RUN, STOPPED, VERDICTS } from "@/components/safety/model";

const params = Promise.resolve({ owner: "acme", repo: "api" });
const orgParams = Promise.resolve({ org: "acme" });
const query = (values: Record<string, string> = {}) => Promise.resolve(values);

async function render(node: ReactNode) {
  const { prelude } = await prerender(node);
  const html = await new Response(prelude as unknown as BodyInit).text();
  const copy = html.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ");
  expect(html.match(/<h1[ >]/g)).toHaveLength(1);
  expect(copy).not.toMatch(/\bsafe\b/i);
  return { html, copy };
}

describe("Safety screens on the fixture world", () => {
  for (const mode of ["keeper", "bee"]) {
    it("renders ordered verdict groups, evidence sources and attention in " + mode, async () => {
      state.mode = mode;
      const { html, copy } = await render(await RepoSafety({ params, searchParams: query() }));
      const headings = [...html.replace(/<!--[\s\S]*?-->/g, "").matchAll(/<h2\b[^>]*>([^<]*)/g)].map(([, title]) => title!.replace(/^## /, "").replace(/ · \d+$/, ""));
      expect(headings.filter((heading) => VERDICTS.some(([, label]) => label === heading))).toEqual([
        "Needs a look", "As asked", "More than asked", "Less than asked", "Something else",
      ]);
      const verdicts = [...html.matchAll(/data-testid="safety-verdict"[^>]*>.*?<span>([^<]*)<\/span><\/span>/g)].map(([, verdict]) => verdict);
      expect(new Set(verdicts)).toEqual(new Set(VERDICTS.map(([, label]) => label)));
      expect(html).toContain("<details");
      expect(copy).toContain("Detected ·");
      expect(copy).toContain("Reported by first-pass");
      expect(copy).toMatch(/Stopped 1 run.*asked for help/);
      expect(copy).toContain("Mara Okafor answers for it");
      if (mode === "bee") {
        expect(html).toContain('data-bee-page="repo-safety"');
        expect(html).toContain('data-tool="safety_raise_concern" data-kind="write"');
        expect(html).toContain('data-tool="safety_set_verdict" data-kind="needs a person"');
        expect(html).not.toContain("<button");
      } else {
        expect(html).toContain('data-agent-confirm="Confirms this finding and records your reason."');
        expect(html).toContain('data-agent-confirm="Removes this item from your attention list');
      }
    });

    it("renders Asked before Done, review evidence and recording limits in " + mode, async () => {
      state.mode = mode;
      const api = await serverClient();
      const { items } = await api.safety.listSafetyAttention({ repo: { fullName: "acme/api" } });
      const work = items.find((item) => item.tripwire)?.work;
      expect(work?.kind).toBe(RUN);
      const { html, copy } = await render(await RepoSafety({ params, searchParams: query({ work: work!.id, kind: work!.kind }) }));
      expect(copy).toMatch(/Asked .*Prepare a redacted incident summary.*Source.*Done .*Files touched.*Merged pull requests/);
      expect(copy.match(/Not recorded by the runner/g)).toHaveLength(2);
      expect(copy).toMatch(/Independent review.*Review submitted.*first-pass.*claude/);
      if (mode === "bee") {
        expect(html).toContain('data-bee-page="safety-record"');
        expect(html).not.toContain("<button");
      } else {
        expect(html).toContain('id="safety-override"');
        expect(html).toContain("Ask for an independent review");
      }
    });

    it("renders organisation findings and oversight in " + mode, async () => {
      state.mode = mode;
      const { html, copy } = await render(await OrgSafety({ params: orgParams }));
      expect(copy).toMatch(/By signal .*harm_to_others.*outside_task.*By agent and swarm.*Tripwires.*Stopped 1 run.*Median time to first human resolution/);
      if (mode === "bee") {
        expect(html).toContain('data-tool="safety_stop_everything" data-kind="needs a person"');
        expect(html).not.toContain("<button");
      } else {
        expect(html).toContain('data-agent-confirm="Stops running agent work and freezes agent merges until resumed. A person can still merge."');
      }
    });
  }

  it("links an attributed pull request and leaves unrelated work out", async () => {
    const api = await serverClient();
    const store = getFixtureStore();
    const merged = store.crByKey("histogram");
    await expect(pullSafetyLink(api, "acme/api", { id: merged.id, number: String(merged.number) })).resolves.toEqual({
      href: expect.stringMatching(/\/acme\/api\/safety\?work=.+&kind=SAFETY_WORK_KIND_SWARM$/),
      verdict: "As asked",
    });
    await expect(pullSafetyLink(api, "acme/api", { id: "unrelated", number: "999" })).resolves.toBeUndefined();
  });

  it("loads a qualifying server sample and renders a verdict control for its work", async () => {
    state.mode = "keeper";
    const store = getFixtureStore();
    const merged = store.crByKey("histogram");
    const original = merged.reviews;
    // The normal fixture merge carries a human approval, so it is outside the sample.
    merged.reviews = original.map((review) => ({ ...review, reviewer: store.p("reviewer") }));
    try {
      const { html, copy } = await render(await RepoSafety({ params, searchParams: query({ sample: String(Date.now()) }) }));
      expect(html).toContain('aria-label="Sampled work"');
      expect(copy).toMatch(/Sample from the last 30 days.*diagnose the p99 spike.*Set the verdict yourself/);
    } finally {
      merged.reviews = original;
    }
  });

  it("shows a persisted organisation stop on every Safety screen and clears it on resume", async () => {
    const api = await serverClient();
    const input = { org: "acme", reason: "Verify the stop banner" };
    const attention = await api.safety.listSafetyAttention({ repo: { fullName: "acme/api" } });
    const work = attention.items.find((item) => item.tripwire)!.work!;
    await api.safety.stopEverything(input);
    try {
      for (const mode of ["keeper", "bee"]) {
        state.mode = mode;
        for (const page of [
          await RepoSafety({ params, searchParams: query() }),
          await RepoSafety({ params, searchParams: query({ work: work.id, kind: work.kind }) }),
          await OrgSafety({ params: orgParams }),
        ]) {
          const { html } = await render(page);
          expect(html).toContain(STOPPED);
          expect(html.indexOf(STOPPED)).toBeLessThan(html.indexOf("<h1"));
        }
      }
    } finally {
      await api.safety.resumeEverything({ org: "acme", reason: "Finish the stop banner test" });
    }
    expect((await render(await OrgSafety({ params: orgParams }))).html).not.toContain(STOPPED);
  });
});
