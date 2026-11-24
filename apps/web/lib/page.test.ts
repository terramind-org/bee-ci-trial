import { isValidElement } from "react";
import { describe, expect, it, vi } from "vitest";
import { BeeError } from "./api/transport";

vi.mock("./api/server", () => ({ sessionToken: async () => "token" }));
vi.mock("next/navigation", () => ({ notFound: () => { throw new Error("NEXT_NOT_FOUND"); } }));

const { guardSection } = await import("./page");

describe("guardSection", () => {
  it("shows a plain card for an unimplemented section, naming it in words and never the RPC", async () => {
    const el = await guardSection("Apps", async () => {
      throw new BeeError("unimplemented", { reason: "UNIMPLEMENTED", message: "AppService/ListApps is not implemented" });
    });
    expect(isValidElement(el)).toBe(true);
    const props = (el as { props: { what: string; message: string } }).props;
    expect(props.what).toBe("Apps");
    expect(props.message).not.toMatch(/Service|\//);
  });

  it("renders the section when it loads, and sends not-found to the 404 page", async () => {
    expect(await guardSection("Apps", async () => "ok")).toBe("ok");
    await expect(guardSection("Apps", async () => { throw new BeeError("not_found", { reason: "ORG_NOT_FOUND", message: "no org" }); })).rejects.toThrow("NEXT_NOT_FOUND");
  });
});
