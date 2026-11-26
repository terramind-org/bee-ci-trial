/**
 * Runs before every page request (Next 16 renamed `middleware.ts` to `proxy.ts`).
 *
 *   - Legacy root-level repo URLs (`/changes/…`, `/code?…`, …) answer 308 to the same page under
 *     `/{owner}/{repo}` from the repo picker's cookie, keeping the query; `/settings/…` goes to
 *     `/orgs/{org}/settings/…`. Without a remembered repo, to the repository list.
 *   - `/{owner}/{repo}/changes/{id}?rev=…&since=…` answers 307 to the change's Code tab.
 *   - Every other page gets the path in a request header, so layouts can send a signed-out
 *     visitor to sign-in and back, and the root layout can read `?mode=`.
 *   - `?mode=bee` or `?mode=keeper` is remembered in a cookie, so a preview of the agent's view
 *     (lib/mode.ts) lasts across pages until the person switches back.
 */
import { NextResponse, type NextRequest } from "next/server";
import { motionReviewEnabled } from "./components/bees/review-access";
import { REPO_COOKIE } from "./lib/api/config";
import { MODE_COOKIE, MODE_COOKIE_MAX_AGE, MODE_PARAM, isMode } from "./lib/mode";
import { PATH_HEADER, legacyRedirect } from "./lib/routes";
import { changeCodePath } from "./lib/change-page";

function cookieRepo(req: NextRequest): string | undefined {
  const raw = req.cookies.get(REPO_COOKIE)?.value;
  if (!raw) return undefined;
  try {
    return decodeURIComponent(raw);
  } catch {
    return undefined;
  }
}

export function proxy(req: NextRequest) {
  const { pathname, search } = req.nextUrl;
  // Answer before the shell can stream its loading state or redirect to sign-in.
  if (/^\/dev\/bees\/?$/.test(pathname) && !motionReviewEnabled()) {
    return new NextResponse("Not found", { status: 404 });
  }
  const target = legacyRedirect(pathname, search, cookieRepo(req));
  if (target) return NextResponse.redirect(new URL(target, req.url), 308);
  // `?rev` and `?since` on a change request belong to its Code tab (docs/spec/web-ui.md §3); the
  // Brief also redirects, but only once it streams, so a client without JavaScript gets it here.
  const code = changeCodePath(pathname, req.nextUrl.searchParams);
  if (code) return NextResponse.redirect(new URL(code, req.url), 307);
  const headers = new Headers(req.headers);
  headers.set(PATH_HEADER, pathname + search);
  const res = NextResponse.next({ request: { headers } });
  const mode = req.nextUrl.searchParams.get(MODE_PARAM);
  if (isMode(mode)) res.cookies.set(MODE_COOKIE, mode, { path: "/", maxAge: MODE_COOKIE_MAX_AGE, sameSite: "lax" });
  return res;
}

export const config = {
  // API routes, build output and static files pass straight through.
  matcher: ["/((?!api/|_next/|icon\\.svg|manifest\\.webmanifest|favicon\\.ico|llms\\.txt).*)"],
};
