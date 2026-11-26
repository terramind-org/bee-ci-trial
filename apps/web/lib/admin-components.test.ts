import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { isCidr } from "../components/admin/cidr";
import { ReadinessList, checksFrom, summarize } from "../components/admin/readiness-list";
import { StatusList, healthOf, uptime, worse } from "../components/admin/status-list";
import type { OpsConfigError } from "./api/types";

const err = (id: string, keyPath: string, message: string): OpsConfigError => ({ keyPath, line: 0, column: 0, message, code: "production:" + id });

describe("status list", () => {
  const items = [
    { name: "metadata_store", status: "ok" as const, detail: "primary responding in 3 ms", critical: true },
    { name: "search_index", status: "degraded" as const, detail: "indexing lag 38s", reason: "index_lagging" },
    { name: "git_gateway", status: "down" as const, detail: "not accepting pushes", reason: "listener_closed" },
  ];

  it("draws one row per component with a dot that also speaks", () => {
    const html = renderToStaticMarkup(h(StatusList, { items, label: "Components" }));
    expect(html).toContain('aria-label="Components"');
    expect(html.match(/<li /g)).toHaveLength(3);
    expect(html).toContain("bg-ok");
    expect(html).toContain("bg-warn");
    expect(html).toContain("bg-bad");
    expect(html).toContain(">Operational<");
    expect(html).toContain(">Degraded<");
    expect(html).toContain(">Down<");
    expect(html).toContain("critical path");
    expect(html).toContain("indexing lag 38s");
  });

  it("shows the reason only when a component is not ok", () => {
    const html = renderToStaticMarkup(h(StatusList, { items, label: "Components" }));
    expect(html).toContain("index_lagging");
    expect(html).toContain("listener_closed");
    expect(renderToStaticMarkup(h(StatusList, { items: [{ name: "forge", status: "ok", reason: "ignored" }], label: "x" }))).not.toContain("ignored");
  });

  it("says so when nothing was reported", () => {
    expect(renderToStaticMarkup(h(StatusList, { items: [], label: "Components", empty: "No components." }))).toContain("No components.");
  });

  it("maps the wire status, treating anything unreported as down", () => {
    expect(healthOf("HEALTH_STATUS_OK")).toBe("ok");
    expect(healthOf("HEALTH_STATUS_DEGRADED")).toBe("degraded");
    expect(healthOf("HEALTH_STATUS_DOWN")).toBe("down");
    expect(healthOf("HEALTH_STATUS_UNSPECIFIED")).toBe("down");
    expect(worse("ok", "degraded")).toBe("degraded");
    expect(worse("down", "degraded")).toBe("down");
  });

  it("prints uptime in its two largest units", () => {
    expect(uptime(0, 26 * 3600_000)).toBe("1d 2h");
    expect(uptime(0, 5 * 3600_000 + 12 * 60_000)).toBe("5h 12m");
    expect(uptime(0, 42_000)).toBe("42s");
  });
});

describe("readiness list", () => {
  const res = {
    errors: [err("tls", "tls.cert_file", "set tls.cert_file or an https public URL")],
    warnings: [err("logging", "logging.level", "logging.level is debug")],
  };

  it("shows the checks the server did not report as passed, and the ones it did as warn or fail", () => {
    const checks = checksFrom(res, "enterprise");
    expect(checks.find((c) => c.id === "tls")?.status).toBe("fail");
    expect(checks.find((c) => c.id === "logging")?.status).toBe("warn");
    expect(checks.find((c) => c.id === "blobs_s3")?.status).toBe("pass");
    expect(summarize(checks)).toBe("1 failed, 1 warning, 11 passed");
  });

  it("only includes the sign-in check for the edition", () => {
    const ids = (e: "cloud" | "enterprise") => checksFrom({ errors: [], warnings: [] }, e).map((c) => c.id);
    expect(ids("enterprise")).toContain("edition_sso");
    expect(ids("enterprise")).not.toContain("edition_login");
    expect(ids("cloud")).toContain("edition_login");
    expect(ids("cloud")).not.toContain("edition_sso");
  });

  it("renders pass, warn and fail with the message and a link into the self-hosting guide", () => {
    const html = renderToStaticMarkup(h(ReadinessList, { checks: checksFrom(res, "enterprise") }));
    expect(html).toContain('data-check="tls" data-status="fail"');
    expect(html).toContain('data-check="logging" data-status="warn"');
    expect(html).toContain('data-check="backups" data-status="pass"');
    expect(html).toContain(">Fail<");
    expect(html).toContain(">Warning<");
    expect(html).toContain(">Pass<");
    expect(html).toContain("set tls.cert_file or an https public URL");
    expect(html).toContain("docs/self-hosting.md → TLS");
    expect(html).toContain("self-hosting.md#tls");
    expect(html).toContain("self-hosting.md#observability");
  });

  it("offers a fix only for what is not passing", () => {
    const html = renderToStaticMarkup(h(ReadinessList, { checks: checksFrom({ errors: [], warnings: [] }, "enterprise") }));
    expect(html).not.toContain("Fix:");
    expect(html).not.toContain(">Fail<");
  });

  it("keeps an item this build does not know, in the server's words", () => {
    const checks = checksFrom({ errors: [err("brand_new", "x.y", "x.y must be set")], warnings: [] }, "enterprise");
    const added = checks.find((c) => c.id === "brand_new");
    expect(added?.status).toBe("fail");
    expect(added?.message).toBe("x.y must be set");
  });
});

describe("cidr", () => {
  it("accepts addresses and blocks of both families", () => {
    for (const ok of ["10.0.0.1", "10.0.0.0/8", "203.0.113.0/24", "0.0.0.0/0", "2001:db8::/32", "::1", "fe80::1/128", "2001:db8:0:0:0:0:0:1", "::ffff:192.0.2.1"]) expect(isCidr(ok), ok).toBe(true);
  });

  it("rejects what the server would", () => {
    for (const bad of ["", "10.0.0", "10.0.0.256", "10.0.0.0/33", "10.0.0.0/", "10.0.0.0/8/8", "2001:db8::/129", "2001::db8::1", "not-an-ip", "1.2.3.4/x", ":::"]) expect(isCidr(bad), bad).toBe(false);
  });
});
