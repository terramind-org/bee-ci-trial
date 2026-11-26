# Packages and Pages (`bee-forge`)

PRODUCT §4.17. Registries for OCI containers, npm, Maven/Gradle, NuGet, RubyGems and Cargo with one
package model; static sites from a ref or a workflow artifact with custom domains, certificates and
private sites. Serves `PackageService` and `PagesService`, plus the registry wire protocols and the
site-serving function as transport-agnostic functions. Conventions: `docs/spec/forge-core.md` §9.

## 1. Layout

| path | owns |
|---|---|
| `src/packages/manage.rs`, `lifecycle.rs` | store: packages, versions, tags, grants, downloads, uploads, retention, purge |
| `src/packages/access.rs` | authorization |
| `src/packages/registry.rs` + `oci.rs`, `npm.rs`, `maven.rs`, `nuget.rs`, `rubygems.rs`, `cargo.rs` | `PackageRegistry::handle(http::Request<Bytes>) -> http::Response<Bytes>` |
| `src/packages/svc.rs` | `PackageService` |
| `src/pages/` | `site`, `deploy`, `domains`, `serve`, `listener`, `svc`, `ports` |

Surfaces: `packages` and `pages` (own pools). Tables `forge_package_*`, `forge_pages_*`, migrations
`packages/0001_create_packages`, `packages/0002_create_pages`. Audit prefixes `package.` and `pages.`.

## 2. Package model

- A package is `(owner, ecosystem, normalized name)`; the owner is an org or a user. Names are
  validated per ecosystem (`INVALID_ARGUMENT`/`bad_name`) and compared by a normalized key (npm,
  container, NuGet, RubyGems lower case; Cargo also `-` = `_`; Maven exact).
- A version is unique by its key (`bad_version` for strings the ecosystem rejects). Containers: every
  manifest is a version keyed by digest, tags are tags. npm dist-tags and container tags share the
  tag table; setting a tag moves it off other versions.
- Files are content in the blob port (`pkg/<owner id>/sha256/<hex>`, or the staging key of a native
  upload), reference counted per owner; the last reference dropped deletes the blob.
- Downloads are counted per package, per version and per day (`GetPackageUsage.since`).
- Deletion is soft for 30 days (`restore_window_days`): packages and versions are restorable; names
  of a deleted package can be reused (restore is then `ALREADY_EXISTS`); a deleted version string
  cannot be republished (`version_deleted`). The maintenance pass purges rows and blobs after the
  window. A public package (or the last version of one) with more than 5000 downloads cannot be
  deleted (`INVALID_STATE`).
- Retention policies per package or owner default (`keep_last`, `untagged_days`, `older_than_days`;
  tagged versions are kept by `keep_last` and `untagged_days`) delete versions (restorable) in the
  maintenance pass; unreferenced container blobs older than a day are collected.
- Owner settings: `forbid_public` (`POLICY_DENIED` when publishing public or changing visibility to
  public) and `storage_quota_bytes` (`QUOTA_EXCEEDED`).
- Events: `package.created/published/updated/deleted/restored/visibility_changed/linked/unlinked/
  access_granted/access_revoked/version_deleted/version_restored/version_updated/tags_updated/
  retention_applied`. Every state change writes an audit entry; denied admin calls are audited.

### 2.1 Permissions

Effective package role per principal = highest of: ownership (user owner; org owner/admin) = admin;
explicit grants (principal, team through the `TeamMembership` port, repo); repo inheritance for a
linked package with inheritance on (read/triage = read, write/maintain = write, admin = admin);
visibility (public readable by anyone including anonymous; internal by org members). Every principal
of the delegation chain must hold the role. The token must allow `read`, `commit` or
`manage_settings` for read, write and admin; a package linked to a repo also requires the token's
repo/path scope to admit the repo; a repo-restricted token otherwise counts only repo grants, and a
repo grant needs the caller to hold read (write for a write grant) on the grantee repo, so attenuating
a token never widens access. Without a role a private package is `NOT_FOUND`.

| RPC | needs |
|---|---|
| reads, versions, provenance, usage | read |
| native publish, tags | write |
| delete/restore package or version, visibility, link, grants | admin |
| create (first publish) | user owner, org owner/admin, or write on the repo it links to (token restricted to repos must name that repo) |

Job tokens: the workflow job token (read + commit, scoped to the repo) is the `packages: write`
credential. Creating a package from a job needs the repo link: `X-Bee-Repo: org/name`, the npm
`repository` field, an OCI `org.opencontainers.image.source` label, or the `repository` field of a
crate / NuGet repository element / gemspec source URI naming a repo of this forge.

## 3. Registry protocols

`PackageRegistry::handle` dispatches by prefix (`packages::mounts`): `/v2/` OCI, `/npm/<owner>/`,
`/maven/<owner>/`, `/nuget/<owner>/`, `/rubygems/<owner>/`, `/cargo/<owner>/`. Credentials: Bearer,
Basic password (or user when the password is empty), `X-NuGet-ApiKey`, or the bare `Authorization`
value (Cargo, RubyGems); verified like gRPC tokens. Anonymous requests read public packages; for a
missing or non-public package they get 401, not 404. Provenance claims travel in `X-Bee-Run-Id`,
`-Workflow`, `-Job`, `-Repo-Id`, `-Ref`, `-Changeset`, `-Signer`, `-Issuer`, `-Attestations`; they
are recorded unverified unless the `ProvenanceVerifier` port confirms them.

- **OCI** (distribution v1.1): blobs HEAD/GET/DELETE; uploads POST (monolithic `?digest=`, mount
  `?mount=&from=`), PATCH (ordered, `Content-Range` checked, 416 otherwise), PUT `?digest=` (digest
  verified, `DIGEST_INVALID`), status, DELETE; manifests PUT/GET/HEAD/DELETE for OCI and Docker v2
  manifests, indexes and lists (children and blobs must exist: `MANIFEST_BLOB_UNKNOWN`), `subject` and
  `OCI-Subject`; `tags/list` with `n`/`last` and `Link`; `referrers/<digest>` with `artifactType`.
  `/v2/` answers 401 with a Basic challenge without credentials. `_catalog` is not served.
- **npm**: publish (`PUT /<pkg>` with `_attachments`, `dist-tags`, `access`, `repository`),
  packument and abbreviated install metadata, version/tag documents, tarballs, dist-tag endpoints,
  deprecation through `PUT` with `deprecated`, unpublish through `PUT .../-rev/<rev>` or `DELETE`
  (allowed within 72 hours or below 300 downloads, else 403 advising deprecation), `whoami`, `ping`.
  Republishing a version is 403.
- **Maven**: `PUT/GET/HEAD` of files under `<group path>/<artifact>/<version>/`, checksums verified
  on PUT and computed on GET, generated `maven-metadata.xml` (artifact level with latest/release/
  lastUpdated in Maven version order; `-SNAPSHOT` level with snapshotVersions), releases immutable
  (identical re-deploys accepted), snapshots overwritable, `DELETE` of version or artifact.
- **NuGet v3**: service index, push (multipart or raw; `.nuspec` read from the package), normalized
  versions, flat container, registration with dependency groups, search, delete (version removal).
- **RubyGems**: push (`.gem` tar, gemspec read), download, yank/unyank, compact index (`versions`,
  `info/<gem>` with checksum, ruby and rubygems requirements, ETag), JSON endpoints, `specs.4.8.gz`,
  `latest_specs.4.8.gz`, `prerelease_specs.4.8.gz` (Ruby Marshal 4.8).
- **Cargo sparse**: `config.json` (`auth-required` by default), index files in the standard layout
  with ETag/304, publish, yank/unyank, download, minimal search.

Native publish (`BeginPackageUpload` / `FinalizePackageVersion`) gives signed upload URLs per file,
verifies size and SHA-256 on finalize (`MISSING_OBJECT`, `INTEGRITY`) and makes the version visible.
Per-ecosystem conventions for natively published files (so the protocols can serve them): npm
`<name>-<version>.tgz`, Cargo `<name>-<version>.crate`, NuGet `<id>.<version>.nupkg` (lower case),
RubyGems `<name>-<version>.gem`; `metadata_json` is the package.json, crate publish metadata,
`{id, version, authors, description, dependencyGroups}`, `{name, version, platform, dependencies}`.

## 4. Pages

- One site per repo. Build type REF (a ref and folder, built into a deployment) or WORKFLOW
  (artifact deployments). Access PUBLIC or PRIVATE (readers of the repo); the org can forbid public
  sites (`POLICY_DENIED`). URL `https://<org>.<pages domain>/<repo>/`, or the custom domain.
- A deployment is a content-addressed manifest (path, SHA-256 blob id, size, content type, parsed
  `_redirects`); identical files and manifests are stored once. The live site is the single
  `current_deployment_id`; switching it is one transaction, so requests see the old or the new site.
  A failed build is recorded as a FAILED deployment (site status ERRORED) and the old site keeps
  serving. Builds complete inside the call, so stored deployments are never QUEUED/BUILDING.
  Unchanged refs return the live deployment. The newest 10 deployments are kept.
- Previews: `deploy_change_preview(repo, number, head)` (REF sites; also on `cr.opened`, `cr.revised`,
  `cr.reopened` events, removed on `cr.closed`/`cr.landed`) serves `https://<repo>-pr-<n>.<org>.<pages
  domain>/`; a new revision replaces it; previews expire after 14 days (per site setting). Workflow
  sites deploy previews from artifacts (`preview = true`). A push to the source ref rebuilds REF sites.
- Domains: custom domain claims are unique per host; an org that verified a domain (TXT
  `_beehive-pages-challenge-<org>.<domain>`) excludes other orgs from it and its subdomains
  (`PERMISSION_DENIED`). `VerifyPagesDomain` failing the lookup is a FAILED verification.
- Certificates: issued through the `AcmeIssuer` port once DNS points at Pages (CNAME to
  `<org>.<pages domain>` or A records at the Pages addresses) and no CAA record excludes the issuer;
  the apex/`www` counterpart joins when it points too; failures are recorded as ERRORED; renewal runs
  30 days before expiry; enforce-HTTPS needs an issued certificate for a custom domain.
- Serving (`pages::serve::serve`, pure): HTTPS redirect; path cleaning (`..`, NUL, dot segments other
  than `.well-known` are 404); `_redirects` (exact, `/*` with `:splat`, 301/302/303/307/308, 200
  rewrite, `!` force); `index.html`, `<path>.html`, folder slash redirect; `404.html`; `Cache-Control:
  max-age=600`, strong ETag with 304.
- Permissions: read for reads, write (`commit`) for builds and deployments (a job token is the
  `pages: write` credential), admin for site, domain and certificate changes; verified-domain RPCs
  need org owner/admin.
- Events: `pages.site_created/updated/deleted`, `pages.deployment_succeeded/failed/cancelled`,
  `pages.preview_created/removed`, `pages.domain_set`, `pages.domain_verified`,
  `pages.certificate_issued/failed`, `page_build.completed`.

## 5. Ports and mount points the server must provide

| port | trait | notes |
|---|---|---|
| blobs | `packages::BlobPort` (`Hooks.packages.blobs`) | get/put/head/delete by key over `bee-store-blob`, under `pkg/` and `pages/` |
| URLs | `community::ports::UrlSigner` (`Hooks.packages.signer`) | signed PUT/GET for native publish and download URLs |
| teams | `packages::TeamMembership` | team grants (enterprise teams) |
| provenance | `packages::ProvenanceVerifier` | verify claims against runs and attestations |
| VCS | `community::ports::ContentVcs` (`Hooks.community.vcs`, or `Hooks.pages.vcs`) | resolve, tree, read |
| DNS | `pages::DnsResolver` | TXT, CNAME, A, AAAA, CAA |
| ACME | `pages::AcmeIssuer` | issues certificates; the TLS terminator reads the key material |
| artifacts | `pages::ArtifactSource` | workflow artifacts |
| gate | `pages::DeployGate` | `github-pages` environment protection (`APPROVAL_REQUIRED`) |

HTTP mounts: `PackageRegistry::new(forge).handle(req)` for the prefixes of `packages::mounts` (the
OCI one at the root of its host); `forge.pages.respond(caller, &req, https)` for the Pages listener
(host-based; `caller` from the session layer for private sites). `ForgeServices::register` adds
`PackageService` and `PagesService`. `Forge::maintain` purges, applies retention and renews certs.

## 6. Real clients

`tests/packages/client.rs` has opt-in tests with `npm`, `cargo` and `oras` (`BEE_TEST_CLIENTS=1`,
`BEE_TEST_REGISTRY`, `BEE_TEST_TOKEN`, `BEE_TEST_OWNER`) for a server that mounts the handlers.

## 7. API gaps

- PyPI has no ecosystem value and is not served.
- The token has no `packages`/`pages` action; `read`, `commit` and `manage_settings` stand in.
  Proposed: `packages_write`, `pages_write`.
- `PagesDeployment.deployment_id` (the `DeploymentService` link) is empty; `PackageVersion.digest`
  is the first file's digest outside containers.
- `RepoRef` in `Package.repo` is returned by full name; org renames change it.
- `GetPackageVersionProvenance.attestation_ids` are claims until a `ProvenanceVerifier` is wired.
- There is no `/v2/_catalog`; NuGet delete removes (not unlists) a version.

Closed (2026-10-19): `PACKAGE_ECOSYSTEM_CARGO = 6`; `GetPackageRetentionPolicy` /
`SetPackageRetentionPolicy` (per package or the owner's default), `GetPackageOwnerSettings` /
`UpdatePackageOwnerSettings` (`forbid_public`, storage quota); Pages preview settings on
`PagesSite` / `UpdatePagesSite`, `ListPagesPreviews`, and `GetPagesOrgSettings` /
`UpdatePagesOrgSettings` (`forbid_public`).
