/** Names shared by the server and the browser. No secrets here. */
export const TOKEN_COOKIE = "bee_token";
export const REPO_COOKIE = "bee_repo";
/** Where `bee-server --dev` listens unless told otherwise. */
export const DEFAULT_API_URL = "http://127.0.0.1:7420";
/** The fixture world's org and repo (BEE_ADAPTER=fixture, unit tests). */
export const FIXTURE_ORG = "acme";
export const FIXTURE_REPO = "acme/api";
/** Browser requests go through the Next route at this prefix (it adds the session token). */
export const BROWSER_BASE = "/api/connect";
