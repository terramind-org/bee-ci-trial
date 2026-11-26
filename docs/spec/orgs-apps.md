# Orgs, enterprise, apps, billing, models, moderation (`bee-forge::enterprise`)

Serves `TeamService`, `EnterpriseService`, `AppService`, `BillingService`, `ModelService`
and `ModerationService` (PRODUCT §4.20, §4.21). Conventions: `docs/spec/forge-core.md` §2-§9.
Five forge modules, each its own surface (failure domain), tables, migrations, event kinds and
audit prefixes:

| module | surface | holds |
|---|---|---|
| `enterprise` | `enterprise` | teams, custom roles, enterprise accounts, IP allow lists, domains, 2FA, SAML SSO, SCIM |
| `enterprise::apps` | `apps` | apps, installations, OAuth, marketplace |
| `enterprise::billing` | `billing` | plans, usage, budgets, invoices, cost centers |
| `enterprise::models` | `models` | model catalog, provider keys, prompt index, evals |
| `enterprise::moderation` | `moderation` | blocks, reports, limits, locks, flood defenses, takedowns |

Cross-domain calls go through traits in `enterprise/common.rs` (`ScopeResolver`, `SpendGate`,
`IpPolicy`, `TeamDirectory`, `TokenRevoker`) and `hooks::WriteGuard`. New dependencies: `ring`
(RSA/ECDSA/Ed25519 verification and signing; already in the lock via rustls), `roxmltree`
(XML), `serde` (JSON bound; pin as in `bee-proto`).

## Wiring the server must do

```rust
let ent = Enterprise::new(forge.clone(), EnterpriseConfig { base_url, dns, two_factor });
ent.attach();                         // team access source + org membership listener
ent.register(&mut routes);            // TeamService, EnterpriseService
let moderation = Moderation::new(forge.clone(), ModerationConfig::new(standing)...);
forge.set_write_guard(moderation.clone());   // issues, comments, CRs honor moderation
let billing = Billing::new(forge.clone(), cfg); let gate: Arc<dyn SpendGate> = billing.clone();
forge.events.add_listener(UsageIngestor::new(billing.clone()));
let apps = Apps::new(forge.clone(), AppsConfig::new(keyring, revoker)
    .with_ip_policy(ent.network.clone()).with_spend_gate(gate.clone())
    .with_sso(ent.network.clone()).with_domains(ent.network.clone()));
apps.attach();
// changes: ChangeHooks { teams: ent.teams.clone(), .. }
// SCIM over HTTP: route /scim/v2/{organizations|enterprises}/.. to ent.scim.handle(..)
// SAML over HTTP: GET metadata ent.sso.sp_metadata(scope); POST ACS -> ent.sso.consume_response
```

Hooks the server calls on every request path:

- **Admission** (`Enterprise::admit` / `admit_caller`): after authentication, before the handler,
  with the call's principals, the orgs touched, the source address and whether the token is an
  app token. Fails `PERMISSION_DENIED`/`principal_blocked` (deprovisioned), `IP_NOT_ALLOWED`,
  `SSO_REQUIRED`, `TWO_FACTOR_REQUIRED`. The `TeamService`/`EnterpriseService` handlers call it
  themselves.
- **Moderation**: `forge.set_write_guard(..)` covers issue create, issue and CR comments and CR
  open (already called in `service/issues.rs`, `changes/service.rs`). Other write paths call
  `Moderation::check_write` (reactions, reviews, assignments, forks, mentions), plus
  `note_open/note_closed`, `record_comment_edit`, `record_outcome`, `is_taken_down`.
- **Billing**: `Billing::enforce` / `SpendGate::check_spend` before metered work, then
  `record_usage` (or a `usage.recorded` event).
- **Custom roles in authorization**: `Roles::repo_actions` / `check_chain` (see API gaps).
- Org joins by managed users: `Accounts::check_can_join`.

## Teams (`teams.rs`)

Tables `forge_teams*` (teams by org with unique slug and parent, members, invitations, repo grants,
review settings and assignment times, IdP group links). A team is also the policy group
`@<org>/<slug>`. Nesting depth <= 8, no cycles (`team_cycle`), secret teams cannot nest
(`secret_team_nesting`), deleting a team moves its children up. Members are `member` or
`maintainer`; members of child teams are inherited members of ancestors; maintainers of a team or
an ancestor manage it. A non-member invitee gets an invitation that becomes a membership when
they join the org (event listener); leaving the org removes team memberships.

Repo access: `TeamAccess` is an `orgs::AccessSource`; a team grants a built-in role or a custom
role id, children inherit the ancestors' grants, `effective_role` reports `team:<slug>[<role>]`.
Review assignment: `pick_reviewers` (round robin by last assignment; load balance by the
caller's outstanding counts; excluded members, author, agents optional), deterministic.
Teams are policy groups: a member of team `<slug>` in org `<org>` (directly or through a child
team) holds the external group `<org>/<slug>` (`Teams::policy_groups_of`), which the server adds
to every principal's groups for policy (`@org/team` in `[groups]`, `allow`, approvals, CODEOWNERS)
and `WhoAmI` (F49-041, cached for `forge.admission_cache_ms`). Team sync: a team linked to IdP groups (`scim` group ids, `saml` group ids from assertions)
follows them (`sync.rs`); manual member changes are `INVALID_STATE` while linked; manual members
stay when groups change. Changes module: `Teams` implements `changes::TeamDirectory`.

## Custom roles (`roles.rs`)

A custom repo role = base (`read|triage|write|maintain`) + permissions from the catalog; it is
rendered as a `[roles.<id>]` policy document and evaluated by `bee_policy::Policy`
(`custom_repo_actions`), so it means exactly what a role in `.beehive/policy.toml` means.
`Roles::repo_actions(repo, principal)` = built-in effective role (any source) plus every custom
role held through teams (inherited) and direct grants; `check_chain` applies the chain rule of
`Authz::repo`. Deleting a role falls holders back to its base. Custom org roles add org
permissions (`manage_teams`, `manage_billing`, `moderate_content`, ...) assigned to principals or
teams (`has_org_permission`). Destructive permissions (`redact`, `purge`, `manage_policy`,
`delete_org`) cannot be in custom roles.

## Enterprise accounts, network controls (`accounts.rs`, `network.rs`)

An enterprise has owners (the last cannot go), orgs, typed policies (enterprise values override
org settings: `members_can_create_teams` is enforced by `TeamService`) and a managed-users flag
(only while every member is SCIM-provisioned; managed users cannot join orgs outside it,
`check_can_join`). Settings of an org or enterprise live in `forge_scope_settings` keyed by scope
(`org:<id>`/`enterprise:<id>`) with a one-row `Flags` summary so admission costs a few point reads.

- **IP allow lists**: IPv4/IPv6 CIDRs; an enterprise list overrides its orgs'; app tokens are
  covered only with `applies_to_installed_apps`; app-declared ranges only with
  `include_installed_app_ranges` (`set_app_ranges`); enabling or editing the list refuses to lock
  out the caller's own address (`would_lock_out`).
- **Domains**: TXT challenge at `_beehive-challenge-<scope>.<domain>` =
  `beehive-challenge=<token>`, 7 days, checked through a `DnsResolver`; one verified owner per
  domain; approved domains skip the check; restricting notifications needs one.
- **2FA**: required flag (+ secure methods only); noncompliant members are blocked at admission or,
  with `remove_noncompliant`, removed (the last owner of an org stays, blocked).

## SAML SSO (`saml.rs`, `sso.rs`)

**What was built.** SAML 2.0 SP: SP metadata, IdP metadata parsing, HTTP-Redirect AuthnRequests
(stored-block DEFLATE), and an XML-DSig verifier written here, because no audited pure-Rust
library exists and `samael` needs libxml2/xmlsec1. It supports only the profile real IdPs emit
and fails closed on everything else: enveloped signature, exclusive c14n, RSA-SHA256/384/512 or
ECDSA-P256-SHA256, SHA-2 digests; one `Reference URI="#ID"`, the referenced element must be the
signature's parent and its `ID` unique (no signature wrapping), the used assertion must be signed
or inside a signed response, keys come only from the configured certificates, DTDs and encrypted
assertions refused, SHA-1 refused. Canonicalization is checked against libxml2 vectors and the
signature against OpenSSL's in unit tests. Semantic checks: status, destination, issuer,
audience, bearer confirmation (recipient, `NotOnOrAfter`, `InResponseTo`), conditions with 2 min
skew, replay cache per assertion id. Not built: encrypted assertions, signed AuthnRequests,
single logout, metadata URL fetching (a worker passes the XML to `update_config`).

Flow: configure (any IdP change clears the test mark) -> `test_config` + test sign-in ->
enforcement allowed (`sso_untested` before) -> logins link an identity (the authenticated
principal that started the sign-in, or the SCIM identity whose userName/email equals the
NameID) and open a session (`session_lifetime_seconds`, capped by the IdP's session end);
sessions end by time or `RevokeSamlSession`. Groups from the assertion drive team sync.

## SCIM 2.0 (`scim.rs`, `scim_filter.rs`)

`Scim::handle(ScimRequest)` serves `/scim/v2/{organizations|enterprises}/<name>/...`:
ServiceProviderConfig, ResourceTypes, Schemas, Users and Groups (GET list with `filter`,
`startIndex`, `count`; GET, POST, PUT, PATCH, DELETE). Filters: `pr eq ne co sw ew gt ge lt le`,
`and/or/not`, parentheses, dotted attributes and value paths. PATCH: add/replace/remove with
paths (`attr`, `attr.sub`, `attr[filter]`, with schema URN prefix), case-insensitive ops,
Entra's string booleans. Bearer tokens are stored hashed, scoped to one org/enterprise, optional
expiry, revocable. A user maps to a principal (created on first provisioning; org members at org
scope); a group to a team (auto-created at org scope with `provisions_teams`, linked explicitly at
enterprise scope). **Deprovisioning** (`active:false` or DELETE) blocks the principal at admission
at once (tokens and agents acting for it stop working) and removes SSO identity/sessions, team
memberships, org memberships and direct repo grants of the scope; reactivation restores
membership and group seats. Every step is audited.

## Apps, OAuth, marketplace (`apps/`)

Tables `forge_apps_*`. Apps from manifests (Ed25519 keys, PKCS#8 PEM shown once, public key
and fingerprint stored; client/webhook secrets hashed except the HMAC key); app JWTs are EdDSA
(`iss` client id or app id, `exp` <= 10 min after `iat`; two dots = JWT, none = Biscuit).
Installation tokens: a Biscuit minted for the installation principal with an explicit repo id set
(never unrestricted; `ALL` enumerates the org's repos) and the actions of the granted permissions,
attenuated to the requested subset; minted ids are tracked and revoked on suspend/uninstall/app
delete; independently `AppsAccess` (an `AccessSource`) grants roles only on covered repos while
the installation is active. An app installed on two repos cannot touch a third at the token,
`Authz` and access-source levels (tested). Per-app webhooks reuse `events::webhooks` (org hook
for `ALL`, repo hooks for `SELECTED`). OAuth: code flow with exact redirect and PKCE S256,
single-use 10 min codes, device flow (`XXXX-XXXX`, 5 s interval, `SLOW_DOWN`), rotating refresh
tokens with family revocation on reuse, user-to-server tokens (chain [app, user], repos =
the app's active installations in the user's orgs). OpenID Connect: the `openid` and `profile`
scopes are accepted, the authorization request's `nonce` is kept with the code
(`Apps::code_nonce`), and the server adds an ES256 `id_token` to the token response
(`docs/spec/server.md`). Marketplace: listings
`DRAFT -> PENDING_REVIEW -> PUBLISHED|REJECTED -> UNPUBLISHED` (auto-approve for self-hosters),
plans (a plan with purchases can only be retired), purchases (paid ones consult `SpendGate`),
publishers and verification.

## Billing (`billing/`)

Tables `forge_billing_*`. Usage is metered per org (`record_usage`, idempotent by key; or
`usage.recorded` events: `{product, sku, quantity, unit, principal_id, repo_id, key}`; agents
attributed through their owner). One deterministic ledger prices a month's rows in `(time, id)`
order (`floor(quantity * unit price)`, plan-included usage consumed first), and queries, budgets,
enforcement and invoices all read it. Budgets (account/org/repo/cost center, product filter,
thresholds fire `billing.budget_threshold_reached` once per month, hard stop). `Billing`
implements `SpendGate`; `enforce` refuses with `QUOTA_EXCEEDED` (`budget_exhausted`,
`spending_limit`). Invoices `INV-<yyyymm>-<seq>` with a lifecycle and signed PDF URLs. With
`enabled=false` (self-hosted) usage and budgets still work; plans/invoices/payment answer
`UNIMPLEMENTED` (`billing_disabled`). Metering rows are not individually audited (volume);
every other change is. Built-in plans: `free`, `team` (self-serve), `enterprise` (contact
sales) and the programs `education` (Team at a 100% discount) and `nonprofit` (Team at 50%):
Team's features and allowances at a lower seat price. Plans that are not self-serve answer
`INVALID_STATE` (`contact_sales`) to account admins and are assigned by a program
administrator (`BillingConfig::program_admins`, the server's `forge.site_admins`) once
eligibility is established outside the product.

## Models (`models/`)

Records only, never on the critical path: catalog, org policy, BYOK key registrations (secret
name only), prompt file index (the VCS-side indexer registers parsed files), eval runs (state
machine, optional check run), usage. Inference RPCs enforce policy and the budget
(`admit_inference`) and then answer `UNIMPLEMENTED/served_elsewhere`; the gateway calls
`record_inference`.

## Moderation (`moderation/`)

`Moderation::check_write` is the enforcement point, in order: takedown (`taken_down`),
`BLOCKED`, `INTERACTION_LIMITED`, `LOCKED`, flood `RESOURCE_EXHAUSTED`; then a first-time
contributor decision (labels, hold, run approval). Blocks cover the whole delegation chain (org,
repo owner, conversation author lists); limits (repo, org, user) judge account age,
contributions or collaboration, agents by their owner; locks stop non-collaborators; flood limits
are per repo (open-CR caps, hourly rates per owner, reputation floor, throttles with expiry and
early release). Reputation = `(landed + 1) / (landed + rejected + 3*spam + 2)`. Hide/minimize
keeps edit history; reports go to repo/org/site queues; takedowns record the audit entry id.
Read set: a few point reads per check (see `moderation/mod.rs`).

## API gaps

- No `CreateEnterprise` RPC: `Accounts::create_enterprise` is a site-admin call.
- `CollaboratorPermission` cannot express custom roles' action sets, and `Authz::repo` evaluates
  built-in roles only: a custom role's extra permissions are visible through `Roles::repo_actions`
  / `check_chain`, not yet through `Authz::repo`. Proposed: an `ActionSource` hook in `Authz`.
- `CreateScimToken` says "revoke with AuthService.Revoke", but SCIM tokens are not Biscuits;
  revocation is `Scim::revoke_token` (no RPC). Proposed: `RevokeScimToken`.
- `SamlSsoConfig` has no field for a metadata fetch result or the test mark, and
  `UpdateSamlSsoConfig` has no way to retest-and-enforce in one call.
- `Principal`/`ScimIdentity` give no field for the managed-user login; the shortcode is applied to
  `Principal.display`.
- `AuditEntry` has no `enterprise` setter in `AuditDraft`; enterprise-scope entries are written
  through `audit_enterprise` (chain `enterprise:<id>`); denied enterprise-scope calls are not
  audited (no org to key them).
- Apps, billing, models and moderation gaps: see each module's docs (`apps/mod.rs`,
  `billing/mod.rs`, `models/mod.rs`, `moderation/mod.rs`); notably no RPC to accept raised app
  permissions, no RPC for org model policy, comment minimizing needs a `CommentSource` to find the
  comment's repo, `ListPromptFiles` ignores the revision.
- The community profile RPC (`GetModerationCommunityProfile`) is `UNIMPLEMENTED/served_elsewhere`
  (community module); audit sinks/exports of `AuditService` are not part of this change.
