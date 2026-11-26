# Beehive Helm chart

Runs Beehive on Kubernetes: the server (several stateless replicas), the web app, their Services,
an optional Ingress, and the usual production wiring (probes, a PodDisruptionBudget, topology
spread, non-root pods with a read-only root filesystem, graceful termination).

**Postgres 17 and an S3-compatible bucket are not in the chart.** They hold every byte of state,
so you bring them: Cloud SQL, RDS, a Postgres operator; GCS, S3, MinIO, rustfs.

| | |
|---|---|
| Chart version | 0.1.0 (appVersion 0.1.0) |
| Kubernetes | 1.27 or newer (tested on 1.36 with kind) |
| Helm | 3 or 4 (tested with 4.2.1) |
| Images | `ghcr.io/terramind-org/bee-server`, `ghcr.io/terramind-org/bee-web`, tag = appVersion |

`scripts/check-helm.sh` installs this chart on a throwaway kind cluster and drives it end to end.
Every statement below that says "tested" is something that script does (see
[What the check runs](#what-the-check-runs)).

## Install

1. **Create the database and bucket.** One Postgres database (the server keeps its tables in the
   schema `config.postgres.schema`, the forge in the schema named in the forge URL below) and one
   bucket that already exists.

2. **Create the Secret.** The chart never creates one, so credentials stay out of Helm values and
   release history. Seven keys (the names can be changed in `secrets.keys`):

   ```sh
   kubectl create namespace bee
   kubectl -n bee create secret generic bee-secrets \
     --from-literal=postgres-url='postgres://bee:<password>@<host>:5432/bee' \
     --from-literal=forge-database='postgres://bee:<password>@<host>:5432/bee?schema=bee_forge' \
     --from-literal=s3-access-key='<access key>' \
     --from-literal=s3-secret-key='<secret key>' \
     --from-literal=signing-key-hex="$(openssl rand -hex 32)" \
     --from-literal=url-key-hex="$(openssl rand -hex 32)" \
     --from-literal=oidc-key-pkcs8-hex="$(openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 | openssl pkcs8 -topk8 -nocrypt -outform DER | xxd -p | tr -d '\n')"
   ```

   Keep the password URL-safe. Three keys are shared by every replica:

   | Key | Signs | Without it |
   |---|---|---|
   | `signing-key-hex` (`auth.signing_key_hex`) | access tokens | tokens from one pod fail on the others |
   | `url-key-hex` (`blobs.url_key_hex`) | signed blob, CI transfer and media URLs | each pod generates its own `url.key`, so a URL issued by one pod is refused by the others |
   | `oidc-key-pkcs8-hex` (`auth.oidc_key_pkcs8_hex`) | the OpenID provider's id_tokens and JWKS, Actions OIDC tokens, attestations | each pod generates its own key, so the JWKS a client fetches does not verify what another pod signed |

   Losing the signing key signs everyone out and changing it invalidates every issued token, so keep
   a copy of all three. The chart sets `blobs.url_key_hex` and `auth.oidc_key_pkcs8_hex` in
   `server.toml` from this Secret. With more than one replica (`server.replicas` above 1, or
   autoscaling with `maxReplicas` above 1) the render **fails** if `secrets.sharedKeys.enabled` is
   false, because the pods would each sign with a key of their own; `false` is for a single replica
   only. `docs/scaling.md` has the full list of what replicas share.

3. **Optional but recommended: an SSH host key**, so every replica presents the same identity to
   `git` over SSH:

   ```sh
   ssh-keygen -q -t ed25519 -N '' -f ssh_host_ed25519_key
   kubectl -n bee create secret generic bee-ssh --from-file=ssh_host_ed25519_key
   ```

4. **Write your values** (everything is documented in [`values.yaml`](values.yaml)):

   ```yaml
   # my-values.yaml
   config:
     publicUrl: https://bee.example.com   # no trailing slash
     s3:
       endpoint: ""            # empty = AWS S3; set it for GCS, MinIO and others
       bucket: bee
     login:
       oidcIssuer: https://id.example.com
       oidcClientId: bee
   secrets:
     existingSecret: bee-secrets
   ssh:
     hostKeySecret:
       name: bee-ssh
   ingress:
     enabled: true
     host: bee.example.com
     tlsSecretName: bee-tls
   ```

5. **Install:**

   ```sh
   helm install bee deploy/helm/bee -n bee -f my-values.yaml
   helm -n bee test bee
   ```

   The chart is not published to a registry yet: install from a checkout. `helm package deploy/helm/bee`
   builds `bee-0.1.0.tgz` (the `ci/` directory is left out).

   `helm test` also needs a short `--timeout` (for example `--timeout 40s`) with Helm 4.2.1, which waits
   for the test pod's deletion until the timeout even though the pod is already gone.

`helm lint` and `helm template` fail with a message that names the missing value when
`config.publicUrl`, `config.s3.bucket` or `secrets.existingSecret` is empty, and
`values.schema.json` rejects wrong types, unknown keys, an unknown edition and fewer than one
replica.

### First admin

Principal ids are created when someone first signs in, so the first site admin takes two steps:

1. Set a sign-in method (`config.login.oidcIssuer` and `config.login.oidcClientId`, or
   `config.login.githubClientId`) and install. Sign in once at the web app (or with
   `bee login`).
2. Read your principal id from `AuthService.WhoAmI` and upgrade with it:

   ```sh
   helm upgrade bee deploy/helm/bee -n bee -f my-values.yaml \
     --set 'config.siteAdmins={<your principal id>}'
   ```

   Site admins are also operators: they can back up, restore and reach `/admin`.

## What it creates

| Object | Notes |
|---|---|
| Deployment `<release>-server` | 2 replicas, rolling updates with `maxUnavailable: 0`. A `check-config` init container runs `bee-server --check-config` on the same configuration and environment, so a configuration the binary rejects stops the new pod before it replaces a working one. |
| Deployment `<release>-web` | 2 replicas; talks to the server Service. |
| Service `<release>-server` | Port 7420, `appProtocol: kubernetes.io/h2c` (gRPC, HTTP/JSON, git over HTTP and MCP share the port). |
| Service `<release>-ssh` | Port 2222, git over SSH (`ssh.enabled`). ClusterIP unless you change `ssh.service.type`. |
| Service `<release>-web` | Port 3100. |
| ConfigMap `<release>-server` | `server.toml`, generated from `config.*`. A change to it rolls the pods. `config.existingConfigMap` replaces it with your own. |
| ServiceAccount | Annotations for workload identity (`serviceAccount.annotations`); the token is not mounted. |
| PodDisruptionBudgets | `maxUnavailable: 1` for the server and the web app. |
| HorizontalPodAutoscalers | Off. `server.autoscaling.enabled` / `web.autoscaling.enabled` (needs metrics-server). |
| Ingress, optional gRPC Ingress | Off. See [Ingress](#ingress). |
| Pod `<release>-test` | The `helm test` hook: `/readyz`, one RPC and the web home through the Services. |

### Replicas

The server is stateless: Postgres and the bucket hold everything. Every replica takes `auth.signing_key_hex`,
`blobs.url_key_hex` and `auth.oidc_key_pkcs8_hex` from the same Secret. Each protected ref has one coalescer owner at a time, a fenced lease in
Postgres; a land that arrives at another replica is forwarded to the owner at its
`server.advertise_url`, which the chart sets to the pod IP (`http://<pod IP>:7420`, from the
downward API). Replicas must therefore reach each other on port 7420 (do not put a NetworkPolicy
between them that blocks it) and the pod network must be IPv4. If an owner dies, another replica
takes the ref over once the lease (6 s by default) lapses.

`helm upgrade --set server.replicas=3` adds a replica with no other step; the check does it.

### Database sizing

Each server replica holds `metadata.max_connections` (16) connections for the metadata store, 4 for
the server's own tables, and 4 for each forge surface it has used, kept open. `docs/scaling.md`
measured 42 to 89 per replica a few seconds after start and 83 to 110 after a full load run
(three replicas, 1,600 lands); a rolling update runs one pod more than `server.replicas` for a while.
Postgres's default `max_connections` of 100 does not cover two replicas plus that surge pod. Set it
to at least `110 x (server.replicas + 1)`, and more when you autoscale. The test stack in [`ci/`](ci)
uses 400 for 3 replicas. `config.postgres.maxConnections` lowers the metadata pool if you cannot raise
the database limit.

Out of connections, the forge can fail to open at start. The server then serves the version-control
API without it and answers every forge RPC "is not served here", and it does not retry. Such a pod
answers `/readyz` with **503** (`forge unavailable`) while `/healthz` stays 200, so the chart's
readiness probe keeps it out of the Service and `helm upgrade --wait` fails instead of reporting
success; the liveness probe uses `/healthz`, so the pod is not restarted for it. Fix the database limit
and delete the pod (`kubectl delete pod`) to make it try again.

### Termination

The server stops on SIGTERM: the preStop hook sleeps `server.preStopSleepSeconds` (5 s) so the
Service stops sending traffic, then the listener stops accepting, requests in flight get 20 s,
and coalescers release their leases. `terminationGracePeriodSeconds` (60 s) covers all of it. The
rolling restart in the check sends a probe through the Service every 100 ms and none fails.

### Data directory

`/data` is an `emptyDir` (`server.data.sizeLimit`). With Postgres and S3 nothing in it is the
system of record: it holds import staging, restored data directories (`/data/restores/<cell>`) and
the SSH host key when you do not provide one. Mount a claim with `server.data.existingClaim` if
you want restores to survive a pod restart (ReadWriteMany when replicas share it).

## Ingress

One host serves the web app and the API, as `deploy/Caddyfile` does for Compose: the API paths
in `ingress.apiPaths` and git smart HTTP (`/<owner>/<repo>.git/`) go to the server, everything
else to the web app.

Those paths cannot be Kubernetes `Prefix` rules: git's path has a variable owner and repo before
`.git`, and `/bee.v1.<Service>/<Method>` has no path-element boundary after the prefix. The
defaults are therefore regular expressions with `pathType: ImplementationSpecific`, which is what
**ingress-nginx with `nginx.ingress.kubernetes.io/use-regex: "true"`** reads (the chart sets that
annotation, and lifts the body-size limit and raises the proxy timeouts so pushes and event streams
work). Another controller needs the same paths in its own spelling: override `ingress.apiPaths`,
`ingress.gitPath`, `ingress.pathType` and `ingress.annotations`.

**gRPC.** ingress-nginx routes an entire Ingress either to gRPC backends or to HTTP ones, so gRPC
clients (grpcurl, generated stubs) get their own host: set `ingress.grpc.enabled` and
`ingress.grpc.host`; that second Ingress sends everything to the server with
`nginx.ingress.kubernetes.io/backend-protocol: "GRPC"`. gRPC through ingress-nginx needs TLS
(`ingress.grpc.tlsSecretName`), since its HTTP/2 listener is TLS-only. HTTP/JSON, MCP and git, which
the CLI and agents use, need no gRPC host. A gateway that honours `appProtocol: kubernetes.io/h2c`
on the Service (the chart sets it) can serve both from one host.

The check installs ingress-nginx v1.15.1 on its kind cluster, enables both Ingresses and sends
requests through the controller: `/healthz`, an RPC and `git ls-remote` reach the server, `/` and a
repo page reach the web app, `/metrics` is not routed, and a real gRPC call (HTTP/2 over TLS to the gRPC
host) is answered by the server with `grpc-status: 16`. Other controllers are **not** tested; the
paths for them are yours to translate.

## Upgrades

1. Take a backup (`AdminService.CreateBackup`, see `docs/self-hosting.md`) and wait for it.
2. `helm upgrade bee deploy/helm/bee -n bee -f my-values.yaml --set server.image.tag=<new>
   --set web.image.tag=<new>` (or bump `appVersion` by installing a newer chart).
3. Each new server pod runs `--check-config` first, then migrates its schema at start; pods are
   replaced one at a time and a ready pod is never removed before its replacement is ready.
   Read the release notes for schema changes before running two versions side by side.
4. To roll back: `helm rollback bee`. If the new version migrated the schema, restore the
   backup instead.

## Values

[`values.yaml`](values.yaml) documents every key. The ones you will touch first:

| Key | Default | |
|---|---|---|
| `config.publicUrl` | required | URL clients reach Beehive at |
| `config.s3.bucket`, `config.s3.endpoint` | required, `""` | the bucket and its endpoint |
| `secrets.existingSecret` | required | Secret with the seven credentials and shared keys |
| `secrets.sharedKeys.enabled` | `true` | url and OIDC keys from the Secret; `false` only for one replica |
| `config.login.*` | empty | sign-in method |
| `config.siteAdmins` | `[]` | principal ids of site admins |
| `config.forgeLookupDeadlineMs` | `3000` | per-forge lookup deadline in milliseconds (100 to 60000) |
| `config.edition` | `enterprise` | `cloud` only for the hosted service |
| `server.replicas` | 2 | ignored with `server.autoscaling.enabled` |
| `server.resources` | 250m / 512Mi, 2Gi limit | |
| `ssh.hostKeySecret.name` | empty | shared SSH host key |
| `ingress.*` | off | |
| `config.extraToml` | empty | any `server.toml` table the chart does not render |

## What the check runs

`scripts/check-helm.sh` (needs docker, helm, kubectl; downloads a pinned, sha256-verified `kind`):

- `helm lint`, `helm template` for six value sets and a pinned kubeconform container
  (Kubernetes 1.36, strict) over every rendering; the required-value and schema errors above.
- A kind cluster; the images built from the checkout and loaded; Postgres 17, rustfs and a mock
  OIDC provider from [`ci/`](ci); `helm install` (2 server replicas), `helm test`.
- First admin through OIDC, org and repo, a land with the `bee` CLI from a pod in the
  cluster, `git clone` and `git push` over smart HTTP, an SSH banner through the ssh Service,
  the web home, repo page and admin console.
- Pod hardening: uid, read-only root filesystem, no ServiceAccount token, one SSH host key on every pod.
- Every server pod, asked directly by pod IP, serves the same `/.well-known/jwks.json` as the Service (the shared OIDC key).
- `helm upgrade --set server.replicas=3`; 20 concurrent clone, commit, land cycles through the
  Service; all 21 files at the tip exactly once; each replica, asked directly, reports the same main.
- A server pod deleted (time to stop), a rolling restart under a probe every 100 ms, a land afterwards.
- ingress-nginx with both Ingresses (above).
- `helm uninstall` leaves nothing of the release; the cluster is deleted.
