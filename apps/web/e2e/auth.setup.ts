import { expect, test as setup } from "@playwright/test";
import { repoPath, rootToken } from "./helpers";

/** Signs in once with the token `bee-server --dev` printed; the other specs reuse the cookie. */
setup("sign in with the dev server's root token", async ({ page, context }) => {
  // A page that needs a session sends a signed-out visitor to sign-in, remembering where to return.
  await page.goto("/import");
  await expect(page).toHaveURL(/\/signin\?next=%2Fimport$/);
  await page.goto("/signin?next=" + encodeURIComponent(repoPath("activity")));
  await page.getByRole("textbox", { name: "Token" }).fill(rootToken());
  await page.getByRole("button", { name: "Sign in with token" }).click();
  await expect(page).toHaveURL(new RegExp(repoPath("activity") + "$"));
  await expect(page.getByRole("heading", { level: 1, name: "Activity" })).toBeVisible();
  // The token lives only in an httpOnly cookie, never in script-readable storage.
  const cookie = (await context.cookies()).find((c) => c.name === "bee_token");
  expect(cookie?.httpOnly).toBe(true);
  expect(cookie?.sameSite).toBe("Strict");
  expect(await page.evaluate(() => document.cookie)).not.toContain("bee_token");
  expect(await page.evaluate(() => JSON.stringify(localStorage) + JSON.stringify(sessionStorage))).not.toContain(rootToken().slice(0, 24));
  await context.storageState({ path: "test-results/.auth/session.json" });
});
