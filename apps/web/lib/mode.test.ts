import { describe, expect, it } from "vitest";
import { canSwitchMode, isMode, modeCookie, modeParam, resolveMode } from "./mode";

describe("mode", () => {
  it("gives a person the person's view and an agent the agent's", () => {
    expect(resolveMode({ kind: "PRINCIPAL_KIND_HUMAN" })).toBe("keeper");
    expect(resolveMode({ kind: "PRINCIPAL_KIND_AGENT" })).toBe("bee");
    expect(resolveMode({ kind: null })).toBe("keeper");
  });

  it("lets a person preview the agent's view by URL or cookie, the URL winning", () => {
    expect(resolveMode({ kind: "PRINCIPAL_KIND_HUMAN", param: "bee" })).toBe("bee");
    expect(resolveMode({ kind: "PRINCIPAL_KIND_HUMAN", cookie: "bee" })).toBe("bee");
    expect(resolveMode({ kind: "PRINCIPAL_KIND_HUMAN", param: "keeper", cookie: "bee" })).toBe("keeper");
    expect(resolveMode({ kind: null, param: "bee" })).toBe("bee");
  });

  it("never moves an agent out of the agent's view", () => {
    expect(resolveMode({ kind: "PRINCIPAL_KIND_AGENT", param: "keeper", cookie: "keeper" })).toBe("bee");
    expect(canSwitchMode("PRINCIPAL_KIND_AGENT")).toBe(false);
    expect(canSwitchMode("PRINCIPAL_KIND_HUMAN")).toBe(true);
    expect(canSwitchMode(null)).toBe(true);
  });

  it("ignores values it does not know", () => {
    expect(resolveMode({ kind: "PRINCIPAL_KIND_HUMAN", param: "admin", cookie: "1" })).toBe("keeper");
    expect(resolveMode({ kind: "PRINCIPAL_KIND_HUMAN", param: "BEE" })).toBe("keeper");
    expect(isMode("bee")).toBe(true);
    expect(isMode("")).toBe(false);
  });

  it("reads ?mode= from the path the proxy passes on", () => {
    expect(modeParam("/acme/api?mode=bee")).toBe("bee");
    expect(modeParam("/acme/api?state=open&mode=keeper")).toBe("keeper");
    expect(modeParam("/acme/api")).toBeNull();
    expect(modeParam("/acme/api?state=open")).toBeNull();
    expect(modeParam(null)).toBeNull();
  });

  it("writes a cookie the server reads back", () => {
    expect(modeCookie("bee")).toBe("bee_mode=bee; Path=/; Max-Age=31536000; SameSite=Lax");
  });
});
