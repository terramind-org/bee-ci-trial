# Test manifests

Used by `scripts/check-helm.sh` on a throwaway kind cluster. They are not part of the chart
package (`.helmignore`) and are not for production: Postgres and the object store keep their data
in an `emptyDir`, and the mock OpenID Connect provider accepts any login.

| File | What it is |
|---|---|
| `dependencies.yaml` | Postgres 17, rustfs (S3-compatible), a Job that creates the bucket, and a mock OIDC provider, in the release's namespace. Credentials come from the Secret `ci-credentials`, which the script creates with random values. |
| `values.yaml` | Chart values that point the release at those services. |
