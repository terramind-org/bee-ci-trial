import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { BeeHeader } from "../bee-header";
import { ModeSwitch } from "../mode-switch";
import { ThemeSwitch } from "../theme";
import { Button, ButtonLink, buttonClass } from "./button";
import { tw } from "./cx";
import { Modal } from "./dialog";
import { EmptyState } from "./empty";
import { Field, Input, Select, Switch, Textarea } from "./field";
import { Menu, MenuLink } from "./menu";
import { TabLinks } from "./tab-links";
import { Tabs, TabsList, TabsPanel, TabsTab } from "./tabs";
import { TAB, TAB_ON } from "./tabs-style";

// The mode switch refreshes the route after a choice; a server render never gets that far.
vi.mock("next/navigation", async (original) => ({ ...(await original<Record<string, unknown>>()), useRouter: () => ({ refresh: () => {}, push: () => {} }) }));

/** The kit as the server renders it: the markup, names and wiring a person or an agent meets first. */
describe("kit", () => {
  it("lets a caller's class replace the kit's for the same property, and nothing else", () => {
    expect(tw("h-[46px] px-5 rounded-full text-base", "h-7 text-sm")).toBe("px-5 rounded-full h-7 text-sm");
    expect(tw("h-[46px] px-[22px] gap-2", "size-9 px-0")).toBe("gap-2 size-9 px-0");
    // A colour is not a size; a variant styles another state.
    expect(tw("h-[46px] text-base text-ink", "text-ink-3 md:h-9")).toBe("h-[46px] text-base text-ink text-ink-3 md:h-9");
    expect(tw("h-[46px]", undefined)).toBe("h-[46px]");
  });

  it("draws buttons as pills: 46px, 34px small, one solid ink variant and a hairline outline", () => {
    expect(buttonClass("primary")).toContain("h-[46px]");
    expect(buttonClass("primary")).toContain("rounded-full");
    expect(buttonClass("primary")).toContain("bg-accent text-on-accent");
    expect(buttonClass("secondary", "sm")).toContain("h-[34px]");
    expect(buttonClass("secondary")).toContain("shadow-[inset_0_0_0_1px_var(--line)]");
    for (const v of ["secondary", "ghost", "danger"] as const) expect(buttonClass(v)).not.toContain("bg-accent");
  });

  it("renders UI SDK's button, a native one, with the type and disabled state it was given", () => {
    const html = renderToStaticMarkup(h(Button, { variant: "primary", type: "submit", disabled: true }, "Save"));
    expect(html).toMatch(/^<button /);
    expect(html).toContain('data-slot="button"');
    expect(html).toContain('type="submit"');
    expect(html).toContain('disabled=""');
    expect(renderToStaticMarkup(h(Button, null, "Go"))).toContain('type="button"');
    expect(renderToStaticMarkup(h(ButtonLink, { href: "/new", children: "New" }))).toMatch(/^<a [^>]*href="\/new"/);
  });

  it("ties a field's label, hint and error to its control", () => {
    const hinted = renderToStaticMarkup(h(Field, { label: "Session label", htmlFor: "s", hint: "Copied into provenance.", children: h(Input, { id: "s", value: "sub-agent", onChange: () => {} }) }));
    expect(hinted).toContain('<label for="s"');
    const label = /<label [^>]*id="([^"]+)"/.exec(hinted)?.[1];
    const hint = /<p [^>]*id="([^"]+)"/.exec(hinted)?.[1];
    expect(hinted).toContain('aria-labelledby="' + label + '"');
    expect(hinted).toContain('aria-describedby="' + hint + '"');
    expect(hinted).toContain('value="sub-agent"');

    const failed = renderToStaticMarkup(h(Field, { label: "Reason", htmlFor: "r", error: "Say why.", children: h(Textarea, { id: "r" }) }));
    expect(failed).toContain('aria-invalid="true"');
    expect(failed).toContain('role="alert"');
    expect(failed).toContain(">Say why.<");
  });

  it("labels a native select through the field's label", () => {
    const html = renderToStaticMarkup(h(Field, { label: "Expires in", htmlFor: "e", children: h(Select, { id: "e" }, h("option", null, "1 hour")) }));
    expect(html).toContain('<label for="e"');
    expect(html).toMatch(/<select [^>]*id="e"/);
  });

  it("accepts a number where a native input would", () => {
    expect(renderToStaticMarkup(h(Input, { id: "n", type: "number", value: 5, onChange: () => {} }))).toContain('value="5"');
  });

  it("renders a switch as a switch, off until turned on", () => {
    const html = renderToStaticMarkup(h(Switch, { "aria-label": "Weekly digest" }));
    expect(html).toContain('role="switch"');
    expect(html).toContain('aria-checked="false"');
    expect(html).toContain('aria-label="Weekly digest"');
  });

  it("renders tabs with one selected and only its panel", () => {
    const html = renderToStaticMarkup(
      h(Tabs, { defaultValue: "cli" }, h(TabsList, { label: "Format", children: [h(TabsTab, { key: "cli", value: "cli", children: "CLI" }), h(TabsTab, { key: "http", value: "http", children: "HTTP" })] }), h(TabsPanel, { value: "cli", children: "bee land" }), h(TabsPanel, { value: "http", children: "POST" })),
    );
    expect(html).toContain('role="tablist"');
    expect(html).toContain('aria-label="Format"');
    expect((html.match(/aria-selected="true"/g) ?? []).length).toBe(1);
    expect(html).toContain("bee land");
    expect(html).not.toContain("POST");
  });

  it("keeps page tabs as links in a named nav, with the current one marked", () => {
    const html = renderToStaticMarkup(
      h(TabLinks, {
        label: "Settings sections",
        items: [
          { href: "/a", label: "General", active: true },
          { href: "/b", label: "Members", active: false, count: 4 },
        ],
      }),
    );
    expect(html).toContain('<nav aria-label="Settings sections"');
    expect(html).toContain('aria-current="page"');
    expect((html.match(/<a /g) ?? []).length).toBe(2);
    expect(html).toContain(">4<");
  });

  it("names an icon-only menu button and says it opens a menu", () => {
    const html = renderToStaticMarkup(h(Menu, { label: "More actions", labelled: true, triggerClass: "t", trigger: "…", children: h(MenuLink, { href: "/a", children: "A" }) }));
    expect(html).toContain('aria-label="More actions"');
    expect(html).toContain('aria-haspopup="menu"');
    expect(html).toContain('aria-expanded="false"');
    // Closed: the items are not in the page.
    expect(html).not.toContain('href="/a"');
  });

  it("renders an open dialog named by its title, with a close button", () => {
    const html = renderToStaticMarkup(h(Modal, { open: true, onOpenChange: () => {}, title: "Keyboard shortcuts", children: "body" }));
    expect(html).toContain('role="dialog"');
    const title = /<h2 [^>]*id="([^"]+)"/.exec(html)?.[1];
    expect(html).toContain('aria-labelledby="' + title + '"');
    expect(html).toContain('aria-label="Close"');
    expect(renderToStaticMarkup(h(Modal, { open: false, onOpenChange: () => {}, title: "Hidden", children: "body" }))).not.toContain("body");
  });

  it("says an empty place is empty, without a picture or a number", () => {
    const html = renderToStaticMarkup(h(EmptyState, { title: "Safety is not built yet", children: "Nothing here is being checked for you." }));
    expect(html).toContain('role="status"');
    expect(html).toContain("Safety is not built yet");
    expect(html).not.toContain("<svg");
    expect(html.replace(/<[^>]+>/g, " ")).not.toMatch(/\d/);
  });

  it("offers the theme as System, Light and Dark, each button named with its visible word", () => {
    const html = renderToStaticMarkup(h(ThemeSwitch));
    expect(html).toContain('role="group" aria-label="Theme"');
    for (const [name, word] of [
      ["Follow the system theme", "System"],
      ["Switch to light theme", "Light"],
      ["Switch to dark theme", "Dark"],
    ] as const) {
      expect(html).toContain('aria-label="' + name + '">' + word + "<");
      expect(name.toLowerCase()).toContain(word.toLowerCase());
    }
  });

  it("offers a person the agent's view and shows which one is on", () => {
    const html = renderToStaticMarkup(h(ModeSwitch, { mode: "keeper" }));
    expect(html).toContain('aria-pressed="true" aria-label="View pages as a person">Person<');
    expect(html).toContain('aria-pressed="false" aria-label="View pages as an agent sees them">Agent<');
  });

  it("opens the agent's view by telling the agent who it is and whom it answers to", () => {
    const agent = renderToStaticMarkup(h(BeeHeader, { reader: { agent: "billing-fixer", answersTo: "mara" } }));
    expect(agent).toContain("You are an agent: billing-fixer, answering to mara.");
    expect(agent).toContain('href="/llms.txt"');
    // The page below keeps the only h1.
    expect(agent).not.toContain("<h1");
    const person = renderToStaticMarkup(h(BeeHeader, { reader: { agent: null } }));
    expect(person).toContain("You are a person previewing what an agent sees.");
    expect(person).not.toContain("You are an agent");
  });
  it("marks the current link tab in ink with an ink underline, with no class left to undo it", () => {
    // Two colour classes on one element are settled by the stylesheet's order, so the current
    // tab must not carry the resting tab's colours at all.
    for (const off of ["border-transparent", "text-ink-2"]) {
      expect(TAB.split(" ")).toContain(off);
      expect(TAB_ON.split(" ")).not.toContain(off);
    }
    const html = renderToStaticMarkup(h(TabLinks, { label: "State", items: [{ href: "/a", label: "Open", active: true }, { href: "/b", label: "Merged", active: false }] }));
    const open = /<a [^>]*href="\/a"[^>]*>/.exec(html)?.[0] ?? "";
    const merged = /<a [^>]*href="\/b"[^>]*>/.exec(html)?.[0] ?? "";
    expect(open).toContain('aria-current="page"');
    expect(open).toContain("border-ink text-ink");
    expect(open).not.toContain("border-transparent");
    expect(merged).not.toContain("aria-current");
    expect(merged).toContain("border-transparent");
  });
});
