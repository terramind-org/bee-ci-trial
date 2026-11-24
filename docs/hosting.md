# Hosting Beehive on GCP

This runbook describes the Terraform configuration in [`deploy/gcp/`](../deploy/gcp/). It creates one regional GKE Autopilot cluster, a private VPC, regional highly available Cloud SQL for PostgreSQL 17, a GCS bucket used through its S3-compatible XML API, Secret Manager secrets, ingress-nginx with a regional external address, and the local Bee Helm chart. The chart deploys the Bee server and web app.

This is an operator deployment, not a hosted product. It does not add browser GitHub/OIDC sign-in, outbound email or verified invitations, shared (cross-replica) rate limits, signup/abuse controls, a runner or workspace fleet, a cell directory or regional placement service, payment collection, legal pages or data-rights workflows, managed alerting/on-call/status operations, a migration controller, or a tested tenant-isolation and audit-export operation. The cloud web edition alone is not sufficient for a public service; see the [hosted audit](../.terramind/swarm/bee-launch-4/reports/hosted-audit.md).

## Prerequisites

Use Terraform 1.9 or later. The root module pins Google and Google Beta providers to `~> 6.0`, and Helm and Kubernetes providers to `~> 2.0`. The chart requires Kubernetes 1.27 or later; its chart README records testing with Kubernetes 1.36 and Helm 4.2.1. Have `gcloud`, `kubectl`, Helm, OpenSSL, `xxd`, and credentials for the intended GCP project available to the operator.

Enable the APIs corresponding to resources in this configuration before planning: Compute Engine (`compute.googleapis.com`), GKE (`container.googleapis.com`), Cloud SQL Admin (`sqladmin.googleapis.com`), Service Networking (`servicenetworking.googleapis.com`), Secret Manager (`secretmanager.googleapis.com`), Cloud Storage (`storage.googleapis.com`), IAM Service Account Credentials (`iamcredentials.googleapis.com`), Cloud DNS (`dns.googleapis.com`) when `dns_managed_zone` is set, and Service Usage (`serviceusage.googleapis.com`) if the operator enables APIs through Google Cloud.

The Terraform identity needs the ability to manage the resources this module creates: networks, subnet secondary ranges, routers/NAT, addresses, and service-networking peering; GKE clusters and cluster credentials; Cloud SQL instances, databases, and users; Secret Manager secrets and versions; GCS buckets, bucket IAM, HMAC keys, and service accounts; and Helm/Kubernetes resources in the target cluster. In predefined-role terms, assess `roles/compute.networkAdmin`, `roles/container.admin`, `roles/cloudsql.admin`, `roles/secretmanager.admin`, `roles/storage.admin`, `roles/storage.hmacKeyAdmin`, `roles/iam.serviceAccountAdmin`, and `roles/servicenetworking.networksAdmin`; add `roles/dns.admin` only when Terraform manages the DNS A record. API enablement needs `roles/serviceusage.serviceUsageAdmin`. Grant project/IAM administration only if the same identity must grant roles to other identities; it is not a resource in this module.

The Terraform Kubernetes and Helm providers use the active Google client token, so the identity also needs to authenticate to the created cluster and create namespaces, Secrets, and Helm releases. Limit `master_authorized_networks` to the operator networks that may reach the public control-plane endpoint.

## One-time state bootstrap

The repository intentionally does not provide a command for creating the Terraform state bucket. Before enabling remote state, create a separate, access-restricted, versioned GCS bucket in the chosen project. The state bucket must not be the `"${name}-blobs"` application bucket: it contains Terraform state and may expose sensitive resource values.

After the bucket exists, replace the commented block in [`deploy/gcp/providers.tf`](../deploy/gcp/providers.tf) with the real state-bucket name and retain the `gcp` prefix:

```hcl
terraform {
  backend "gcs" {
    bucket = "bee-terraform-state"
    prefix = "gcp"
  }
}
```

The repository documents the backend block but no bucket-creation command; use the approved project bootstrap procedure rather than inventing one here. Restrict access to state because it can contain generated passwords and HMAC credentials.

## First deploy

Work from `deploy/gcp`. Copy [`terraform.tfvars.example`](../deploy/gcp/terraform.tfvars.example) to `terraform.tfvars` and set at least `project_id`, `domain`, `master_authorized_networks`, `database_tier`, `backup_retention_days`, `server_replicas`, `image_tag`, `namespace`, and `tls_secret_name`. Set `dns_managed_zone` only when an existing authoritative Cloud DNS managed zone should receive the A record. Set `install_cert_manager = true` only when using the included cert-manager issuer example.

The OIDC input is deliberately not placed in `terraform.tfvars`; create it as described in [OIDC signing key](#oidc-signing-key). Then initialize, inspect, and apply exactly this root module:

```sh
cd deploy/gcp
terraform init
terraform plan
terraform apply
```

The configuration reserves a regional Premium external IP and exposes it as `ingress_ip_address`. When `dns_managed_zone` is empty, create the `domain` A record at the DNS provider with that output. When it names an existing Cloud DNS zone, Terraform creates the A record with a 300-second TTL.

### TLS

The chart Ingress uses the Secret named by `tls_secret_name`; Terraform does not create the certificate or this Secret. The repository documents this Secret as operator-supplied but does not prescribe a creation command. Create it in `namespace` before relying on the HTTPS endpoint, using the certificate-management procedure approved for the cluster.

Alternatively, set `install_cert_manager = true`. After the cert-manager Helm release exists, copy and edit [`cluster-issuer.example.yaml`](../deploy/gcp/manifests/cluster-issuer.example.yaml), replacing `ops@example.com` with a monitored address, and apply it. This manifest creates the `letsencrypt-production` `ClusterIssuer` with ACME HTTP-01 through the `nginx` ingress class; it does not create a `Certificate`, so a certificate/Secret resource is not documented in this repository. The associated ingress and TLS behaviour are unverified in GCP.

### First administrator

The Terraform chart inputs do not expose a login provider. The chart documentation says that a login method must be configured with `config.login.oidcIssuer` and `config.login.oidcClientId`, or `config.login.githubClientId`; this Terraform module does not document how to pass those values, so that configuration is not documented here.

The first Terraform deploy applies with `site_admins = []`. Once sign-in is configured, use the procedure in [First admin and SSO](self-hosting.md#first-admin-and-sso): sign in once and obtain the principal id from `AuthService.WhoAmI`. Add that id to `site_admins` in `terraform.tfvars`, then update the Terraform-managed release:

```sh
terraform plan
terraform apply
```

Do not set this value on the Helm release by hand: the next Terraform apply overwrites release values that are not in the Terraform configuration. Site administrators are operators and can use backups, restores, and `/admin`.

## OIDC signing key

`oidc_key_pkcs8_hex` is a required sensitive Terraform input. The secrets module generates and stores the OIDC private key in Secret Manager as PKCS#8 PEM, but the chart Secret requires the same key as PKCS#8 DER encoded in hexadecimal. Retrieve the PEM by the approved secret-access procedure into a protected `key.pem`, then set the environment input with the conversion recorded in the module:

```sh
export TF_VAR_oidc_key_pkcs8_hex="$(openssl pkcs8 -topk8 -nocrypt -outform DER -in key.pem | xxd -p | tr -d '\n')"
```

Do not add the resulting value to `terraform.tfvars`, commit it, or print it. Every server replica must receive this one value; otherwise their JWKS and signed OIDC tokens disagree.

## Upgrades and scaling

Take a successful Beehive backup before an upgrade. Change `image_tag` in `terraform.tfvars`, then run `terraform plan` and `terraform apply`. The app module passes the tag to both `ghcr.io/terramind-org/bee-server` and `ghcr.io/terramind-org/bee-web`; it uses the chart's rolling deployment.

Before a rolling server update, ensure Cloud SQL has capacity for the old replicas plus one surge replica. This configuration sets `max_connections` to `110 * (server_replicas + 1)`. Read the release notes before running mixed versions when schemas change, and wait for `/readyz` before the next replacement. A forge that cannot open because of exhausted database connections remains unready.

Scale by changing `server_replicas` and applying Terraform. The same formula changes Cloud SQL `max_connections`; plan that change together with the replica increase. Replicas share Postgres, the GCS bucket, `auth.signing_key_hex`, `blobs.url_key_hex`, and `auth.oidc_key_pkcs8_hex`; each chart pod receives its own `server.advertise_url`. See [Scaling Beehive](scaling.md) for coalescer routing, shared keys, readiness/load-balancer requirements, per-replica rate-limit caveats, and measured connection use.

## Key rotation

Use the Beehive `AdminService.RotateSigningKey` and `AdminService.RetireSigningKey` APIs for the server's signing-key ring. The ring is in shared server metadata and replicas refresh it without a restart; [Scaling Beehive](scaling.md) describes the refresh bound and retirement behaviour.

Terraform generates and stores the initial `token_signing_key`, `blob_url_key`, database password, and OIDC PEM in Secret Manager, and creates the GCS HMAC key. The Terraform module does not document a safe in-place rotation procedure for `url-key-hex`, `oidc-key-pkcs8-hex`, the database password, or the GCS HMAC key. Treat changes to those Terraform-managed values as a coordinated maintenance operation: they are copied into the Kubernetes Secret during apply and must remain identical across replicas.

## Backups and restore

Cloud SQL has automated backups and point-in-time recovery enabled with seven days of transaction-log retention. The application bucket deletes objects only under `backups/` after `backup_retention_days`; it has bucket versioning disabled. Protect the bucket separately, because it contains blob bytes, server backups, and sensitive backup key material.

Use Beehive's own `AdminService.CreateBackup` and `AdminService.StartRestore` procedure in [Backup and restore](self-hosting.md#backup-and-restore). A Beehive backup captures metadata and forge schemas in a repeatable-read transaction, key material in the data directory, and blob manifests; for an S3 target the bucket remains the durable blob copy. Run a drill restore first and require `fsckClean` and `conformancePassed` before treating a backup as recoverable.

Restore on Kubernetes has not been tested. The self-hosting recovery instructions describe Compose volumes and cannot be assumed to apply unchanged to this `emptyDir`-based chart deployment. Cloud SQL PITR and GCS retention are infrastructure recovery mechanisms; they do not prove a Beehive application restore.

## Logs, metrics, and traces

Collect server stderr as structured JSON logs. Set `[logging] level` and `requests`; `requests = true` emits one line per request. Do not expose `/metrics` through the public ingress: scrape it from the server pods or service inside the cluster. Metrics include RPC request/duration, in-flight/admission, land/coalescer, event/audit/secret-finding, and integrity metrics; the complete fixed names are in [the server specification](spec/server.md#6-observability).

For OTLP/HTTP tracing, set these exact server configuration keys:

```toml
[tracing]
otlp_endpoint = "https://<collector-base-url>"
export_interval_ms = 1000
sample_ratio = 1.0
service_name = "bee-server"
```

`otlp_endpoint` is the collector base URL; Beehive posts OTLP/HTTP JSON to `<url>/v1/traces`. The server creates one span per RPC, honors W3C `traceparent`, and exports the `service.name`, `service.instance.id`, and `bee.cell` resource attributes. Its in-process export queue holds 10,000 spans; it drops the oldest when full and drops failed exports. The Terraform app module does not currently expose these keys as chart values, so configure them through the chart's `config.extraToml` only after confirming the rendered configuration does not define `[tracing]` twice.

## G1 production gate

Do not serve customers from this topology until G1 passes. [The plan](PLAN.md#gate-before-production) requires the `bench/db` metadata benchmark—hot ref, ingest, point reads, fault, and operations runs—on at least three machines in separate zones with synchronous replication, recording whether the SPEC §16 land targets hold with the coalescer.

For this GCP topology, provision or select three independent benchmark machines in separate zones and a synchronously replicated database configuration before collecting evidence. On each machine, follow [`bench/db/README.md`](../bench/db/README.md): start the listed benchmark databases with its documented durable settings, provide `FDB_LIB` when FoundationDB is included, run `./run.sh <label>`, then summarize `results/<run>/results.jsonl` with `python3 summarize.py`. Preserve each generated `env.txt` and results directory, record the zone and replication settings with the run, and add the comparison and pass/fail result to [`docs/benchmarks.md`](benchmarks.md). The benchmark scripts are not a Kubernetes deployment test; separately test the deployed server/coalescer under the production load balancer.

## Not verified

No one has run `terraform apply` for this configuration. Offline Terraform validation does not establish GCP IAM/API acceptance, resource provisioning, GKE provider authentication, chart rollout, DNS propagation, ingress routing, TLS Secret handling, cert-manager/ACME issuance, Cloud SQL connectivity, or rollback behaviour.

The server's GCS compatibility path is also untested. With `config.s3.endpoint = "https://storage.googleapis.com"`, its S3 client uses path-style addressing (`https://storage.googleapis.com/<bucket>/<key>`), signs requests and presigned GET URLs with SigV4 using the GCS HMAC access ID and secret, and issues bucket creation, GET (including ranges), HEAD, conditional and ordinary PUT, multipart upload, COPY, DELETE, list, and presigned-GET operations. The application bucket is Terraform-provisioned, so server-side bucket creation is not expected in normal operation; compatibility of the operations above against GCS XML API remains unproven.

The GCP handoffs also leave live Terraform plan/apply, any cloud-resource/API interaction, ingress/TLS behaviour, and the manual OIDC PEM-to-DER-hex transfer unverified. The included issuer example retains `ops@example.com` until an operator edits it. The deployment also has no documented Terraform path for login-provider values or for a cert-manager `Certificate` that produces `tls_secret_name`.
