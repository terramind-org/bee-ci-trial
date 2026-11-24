import { expect, test } from "bun:test";
import { redact, registerSecret } from "../lib/rpc";

test("opaque minted credentials and environment credentials never reach diagnostics", () => {
  const opaque = registerSecret("opaque-test-agent-credential");
  const previous = process.env.EXIT_TEST_ADMIN_TOKEN;
  process.env.EXIT_TEST_ADMIN_TOKEN = "opaque-test-admin-credential";
  try {
    expect(redact(`failed: ${opaque}; opaque-test-admin-credential`)).toBe("failed: ***; ***");
    expect(redact("Authorization: Basic abc123")).toBe("Authorization: Basic ***");
  } finally {
    if (previous === undefined) delete process.env.EXIT_TEST_ADMIN_TOKEN;
    else process.env.EXIT_TEST_ADMIN_TOKEN = previous;
  }
});
