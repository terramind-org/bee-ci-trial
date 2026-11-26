#!/usr/bin/env bash
# shellcheck disable=SC2016,SC2030,SC2031 # jq variables are quoted; KUBECONFIG guards deliberately run in subshells
# End-to-end check of the Helm chart (deploy/helm/bee) on a throwaway kind cluster.
#
#   1. static: `helm lint`, `helm template` for seven value sets, every rendering validated
#      by a pinned kubeconform container, and the required-value and schema errors asserted;
#   2. builds the server and web images from this checkout (BEE_HELM_SKIP_BUILD=1 reuses
#      images of an earlier run), creates the kind cluster `bee-helm`
#      with a pinned kind binary (sha256 verified) and node image, and loads the images;
#   3. applies the test dependencies (deploy/helm/bee/ci: Postgres 17, rustfs, a mock OIDC
#      provider), creates the Secrets, and `helm install`s the chart (2 server replicas);
#   4. becomes the first admin the way docs/self-hosting.md says (OIDC sign-in, the principal id
#      into config.siteAdmins, helm upgrade), creates an org and repo, lands a change with the
#      bee CLI from a pod in the cluster, clones and pushes with git over smart HTTP, GETs
#      the web app, checks the pods' hardening (non-root, read-only root filesystem);
#   5. `helm upgrade --set server.replicas=3`, lands 20 changes concurrently through the
#      Service, asserts all 21 files are in the tip exactly once and that each replica, asked
#      directly, reports the same main;
#   6. deletes one server pod (how long it takes to stop), rolls the Deployment while probing
#      the Service, and asserts no probe failed and nothing was lost;
#   7. `helm uninstall`, asserts nothing is left, deletes the cluster (always, in a trap).
#
# SAFETY: the machine's default kubeconfig may point at production clusters. This script exports
# a KUBECONFIG of its own (a fresh temporary file), creates the cluster into it, and every kubectl
# and helm call goes through a wrapper that first asserts that the current context is
# kind-bee-helm and that its API server is on 127.0.0.1. Nothing else is ever touched.
#
# BEE_HELM_KEEP=1 leaves the cluster and the temporary directory in place, for debugging.
# BEE_HELM_STATIC_ONLY=1 stops after the lint, template and kubeconform checks (no cluster).
# BEE_HELM_SKIP_BUILD=1 reuses the images tagged bee-server:helm-ci and bee-web:helm-ci
# from an earlier run (or, without them, bee-server:latest and bee-web:latest).
# BEE_CHECK_CARGO_BUILD_JOBS sets the image build's rustc parallelism (default 2).
# BEE_CHECK_BUILD_LOCK=<file> runs the image builds under `lockf <file>` (or `flock`).
# BEE_TOOLS_DIR is where the pinned kind binary is cached (default ~/.cache/bee-tools).
# Needs docker, helm 3 or 4, kubectl, curl, git, jq, openssl and ssh-keygen. Without one of them
# it skips with exit 0, unless CI=true. Host ports, on 127.0.0.1: 17520 (server), 13200 (web),
# 18580 (mock OIDC provider), 18480 and 18443 (ingress-nginx).
set -euo pipefail

cd "$(dirname "$0")/.."
NAME=check-helm

for tool in docker helm kubectl curl git jq openssl ssh-keygen; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    if [ "${CI:-}" = "true" ]; then
      echo "$NAME: $tool is required under CI=true" >&2
      exit 1
    fi
    echo "$NAME: $tool is not installed; skipping"
    exit 0
  fi
done
if ! docker info >/dev/null 2>&1; then
  if [ "${CI:-}" = "true" ]; then
    echo "$NAME: the docker daemon is not running (required under CI=true)" >&2
    exit 1
  fi
  echo "$NAME: the docker daemon is not running; skipping"
  exit 0
fi

# ---------------------------------------------------------------------------------------------
# Pinned tools. Every digest and checksum below was taken from the release it names.
# ---------------------------------------------------------------------------------------------
KIND_VERSION=v0.33.0
KIND_NODE_IMAGE='kindest/node:v1.36.4@sha256:099e049362a1526b2db71494e1947aae99bd16290d7c895f2b7ea312e3cbfaed'
KUBECONFORM_IMAGE='ghcr.io/yannh/kubeconform:v0.8.0@sha256:faffaf43f95aa6425306e1ab8d6fcad72acb9049158f38e574c085ea1ec0f64e'
KUBERNETES_SCHEMA_VERSION=1.36.0
INGRESS_NGINX_VERSION=controller-v1.15.1
INGRESS_NGINX_SHA=2a3ae008c8786431115502644e77ab398fdebfb721a5d1195ed3089cde3299df
TOOLS="${BEE_TOOLS_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/bee-tools}"

CLUSTER=bee-helm
CTX="kind-$CLUSTER"
NS=bee-ci
RELEASE=bee
CHART=deploy/helm/bee
CI_DIR="$CHART/ci"
SERVER_IMAGE=bee-server:helm-ci
WEB_IMAGE=bee-web:helm-ci
API_PORT=17520
WEB_PORT=13200
OIDC_PORT=18580
INGRESS_HTTP_PORT=18480
INGRESS_HTTPS_PORT=18443
API="http://127.0.0.1:$API_PORT"
WEB="http://127.0.0.1:$WEB_PORT"
OIDC_HOST="mock-oidc:8080"
OIDC_CLIENT_ID=bee-helm
TIMEOUT_SECS="${BEE_CHECK_TIMEOUT_SECS:-300}"
BUILD_JOBS="${BEE_CHECK_CARGO_BUILD_JOBS:-2}"
START_TIME=$SECONDS

say() { echo "$NAME: $*"; }
die() { echo "$NAME: FAIL: $*" >&2; exit 1; }
port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
rand() { openssl rand -hex "$1"; }
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

TMP="$(mktemp -d "${TMPDIR:-/tmp}/bee-helm.XXXXXX")"
WORK="$TMP/work"
mkdir -p "$WORK" "$TMP/render"

# The only kubeconfig this script ever uses. kubectl, helm and kind all read KUBECONFIG; the
# machine's own configuration (and any context in it) is never consulted.
OWN_KUBECONFIG="$TMP/kubeconfig"
export KUBECONFIG="$OWN_KUBECONFIG"
unset KUBECONTEXT HELM_KUBECONTEXT HELM_KUBECONFIG KUBERNETES_MASTER KUBE_CONFIG_PATH

# An empty or unset KUBECONFIG (or an empty --kubeconfig) makes kubectl and helm fall back to the
# machine's default kubeconfig, which may point at production clusters. Every call that can reach a
# cluster goes through this check first.
require_kubeconfig() {
  [ -n "${KUBECONFIG:-}" ] || die "SAFETY: KUBECONFIG is empty or unset, so kubectl would use the machine's default kubeconfig; refusing to continue"
  [ "$KUBECONFIG" = "$OWN_KUBECONFIG" ] || die "SAFETY: KUBECONFIG is '$KUBECONFIG', not this script's own file $OWN_KUBECONFIG; refusing to continue"
}
assert_own_cluster() {
  require_kubeconfig
  local current server
  current="$(kubectl --kubeconfig "$KUBECONFIG" config current-context 2>/dev/null || true)"
  [ "$current" = "$CTX" ] || die "SAFETY: the current kubectl context is '$current', not $CTX; refusing to continue"
  server="$(kubectl --kubeconfig "$KUBECONFIG" config view --minify -o 'jsonpath={.clusters[0].cluster.server}')"
  case "$server" in
    https://127.0.0.1:* | https://localhost:* | https://\[::1\]:*) ;;
    *) die "SAFETY: context $CTX points at $server, which is not a local kind cluster" ;;
  esac
}
kc() { assert_own_cluster; kubectl --kubeconfig "$KUBECONFIG" --context "$CTX" "$@"; }
hm() { assert_own_cluster; helm --kubeconfig "$KUBECONFIG" --kube-context "$CTX" "$@"; }

# Phase timings, printed at the end (and on failure).
TIMINGS=()
PHASE=""
PHASE_T0=$SECONDS
phase() {
  if [ -n "$PHASE" ]; then
    TIMINGS+=("$(printf '%-36s %5ss' "$PHASE" "$((SECONDS - PHASE_T0))")")
  fi
  PHASE="$1"
  PHASE_T0=$SECONDS
  [ -z "$PHASE" ] || say "== $PHASE"
}
print_timings() {
  [ "${#TIMINGS[@]}" -gt 0 ] || return 0
  echo "$NAME: timings"
  printf '  %s\n' "${TIMINGS[@]}"
  printf '  %-36s %5ss\n' "total" "$((SECONDS - START_TIME))"
}

KIND=""
CLUSTER_CREATED=""
dump_state() ( # a subshell: a failed safety assertion here must not skip the cluster deletion
  echo "$NAME: failed; cluster state follows"
  kc get pods,svc,jobs -A -o wide
  kc -n "$NS" describe pods -l app.kubernetes.io/instance="$RELEASE"
  for pod in $(kc -n "$NS" get pods -o name); do
    echo "---- logs of $pod"
    kc -n "$NS" logs "$pod" --all-containers --tail 60
  done
)
pf_stop() { # name: stops a port-forward and waits until it has exited and freed its port
  [ -f "$TMP/pf-$1.pid" ] || return 0
  local pid waited=0
  pid="$(cat "$TMP/pf-$1.pid")"
  rm -f "$TMP/pf-$1.pid"
  kill "$pid" 2>/dev/null || true
  while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 50 ]; do
    sleep 0.1
    waited=$((waited + 1))
  done
  wait "$pid" 2>/dev/null || true
}
cleanup() {
  status=$?
  trap - EXIT
  set +e
  if [ "$status" -ne 0 ] && [ -n "$PHASE" ]; then
    TIMINGS+=("$(printf '%-36s %5ss (failed)' "$PHASE" "$((SECONDS - PHASE_T0))")")
  fi
  for f in "$TMP"/pf-*.pid; do
    [ -f "$f" ] || continue
    pf_stop "$(basename "$f" .pid | sed 's/^pf-//')"
  done
  if [ -n "${BEE_HELM_KEEP:-}" ]; then
    print_timings
    echo "$NAME: BEE_HELM_KEEP is set; leaving cluster $CLUSTER running (state in $TMP; export KUBECONFIG=$KUBECONFIG)"
    exit "$status"
  fi
  if [ "$status" -ne 0 ] && [ -n "$CLUSTER_CREATED" ] && [ -f "$KUBECONFIG" ]; then
    dump_state >&2 || true
  fi
  if [ -n "$KIND" ] && [ -n "$CLUSTER_CREATED" ]; then
    "$KIND" delete cluster --name "$CLUSTER" >/dev/null 2>&1
  fi
  rm -rf "$TMP" 2>/dev/null
  [ "$status" -eq 0 ] || print_timings
  exit "$status"
}
trap cleanup EXIT

wait_for_code() { # name url expected-code
  local name=$1 url=$2 want=$3 waited=0
  until [ "$(curl -sL -o /dev/null -w '%{http_code}' --max-time 5 "$url" || true)" = "$want" ]; do
    [ "$waited" -lt "$TIMEOUT_SECS" ] || die "$name did not answer $want at $url within ${TIMEOUT_SECS}s"
    sleep 1
    waited=$((waited + 1))
  done
}
assert() { # description, then a command that must succeed
  local what=$1
  shift
  "$@" >/dev/null 2>&1 || die "assertion failed: $what"
  say "ok: $what"
}
refute() { # description, then a command that must fail
  local what=$1
  shift
  if "$@" >/dev/null 2>&1; then die "assertion failed: $what"; fi
  say "ok: $what"
}
locked() { # runs "$@" under BEE_CHECK_BUILD_LOCK when it is set
  if [ -z "${BEE_CHECK_BUILD_LOCK:-}" ]; then
    "$@"
  elif command -v lockf >/dev/null 2>&1; then
    lockf "$BEE_CHECK_BUILD_LOCK" "$@"
  else
    flock "$BEE_CHECK_BUILD_LOCK" "$@"
  fi
}

# ---------------------------------------------------------------------------------------------
phase "static: lint, template, kubeconform"
# ---------------------------------------------------------------------------------------------
REQUIRED=(--set config.publicUrl=https://bee.example.com --set config.s3.bucket=bee --set secrets.existingSecret=bee-secrets)
helm lint "$CHART" "${REQUIRED[@]}" >"$TMP/lint.out" 2>&1 || { cat "$TMP/lint.out" >&2; die "helm lint failed"; }
helm lint "$CHART" -f "$CI_DIR/values.yaml" >"$TMP/lint.out" 2>&1 || { cat "$TMP/lint.out" >&2; die "helm lint with ci/values.yaml failed"; }
say "ok: helm lint is clean ($(grep -c '^\[INFO\]' "$TMP/lint.out") info line(s), no warning or error)"
! grep -qE '^\[(WARNING|ERROR)\]' "$TMP/lint.out" || die "helm lint reported a warning"

render() { # name, helm template arguments
  local name=$1
  shift
  helm template bee "$CHART" --namespace "$NS" "$@" >"$TMP/render/$name.yaml" 2>"$TMP/render.err" \
    || { cat "$TMP/render.err" >&2; die "helm template failed for the '$name' value set"; }
}
render defaults "${REQUIRED[@]}"
render ci -f "$CI_DIR/values.yaml"
render ingress "${REQUIRED[@]}" --set ingress.enabled=true --set ingress.host=bee.example.com \
  --set ingress.tlsSecretName=bee-tls --set ingress.grpc.enabled=true --set ingress.grpc.host=grpc.bee.example.com \
  --set ingress.grpc.tlsSecretName=bee-grpc-tls
render autoscaling "${REQUIRED[@]}" --set server.autoscaling.enabled=true --set web.autoscaling.enabled=true \
  --set server.autoscaling.targetMemoryUtilizationPercentage=80
render minimal "${REQUIRED[@]}" --set secrets.sharedKeys.enabled=false --set web.enabled=false --set ssh.enabled=false --set config.existingConfigMap=my-server-toml \
  --set server.replicas=1 --set server.podDisruptionBudget.enabled=false
cat >"$TMP/everything.values.yaml" <<'EOF'
config:
  edition: cloud
  siteAdmins: ["01ARZ3NDEKTSV4RRFFQ69G5FAV"]
  s3:
    endpoint: https://storage.googleapis.com
  login:
    githubClientId: abc
    defaultOrg: acme
  tracingOtlpEndpoint: http://otel:4318
  extraToml: |
    [limits]
    max_blob_bytes = 1048576
serviceAccount:
  annotations:
    iam.gke.io/gcp-service-account: bee@example.iam.gserviceaccount.com
imagePullSecrets:
  - name: regcred
ssh:
  service:
    type: LoadBalancer
  hostKeySecret:
    name: bee-ssh
server:
  data:
    existingClaim: bee-data
  image:
    digest: sha256:0000000000000000000000000000000000000000000000000000000000000000
  priorityClassName: high
  extraEnv:
    - name: RUST_LOG
      value: info
  tolerations:
    - key: dedicated
      operator: Exists
  nodeSelector:
    pool: bee
web:
  extraEnv:
    - name: NODE_OPTIONS
      value: --max-old-space-size=384
EOF
render everything "${REQUIRED[@]}" -f "$TMP/everything.values.yaml"
RENDERED="$(find "$TMP/render" -name '*.yaml' | wc -l | tr -d ' ')"
say "rendered $RENDERED value sets"

docker run --rm -v "$TMP/render:/manifests:ro" "$KUBECONFORM_IMAGE" \
  -strict -summary -kubernetes-version "$KUBERNETES_SCHEMA_VERSION" /manifests >"$TMP/kubeconform.out" 2>&1 \
  || { cat "$TMP/kubeconform.out" >&2; die "kubeconform rejected a rendering"; }
say "ok: kubeconform (Kubernetes $KUBERNETES_SCHEMA_VERSION, strict): $(tail -1 "$TMP/kubeconform.out")"

# A rendering has the right shape; these are the properties the chart promises.
assert "the server Deployment runs 2 replicas by default" \
  grep -q '^  replicas: 2$' <(helm template bee "$CHART" "${REQUIRED[@]}" --show-only templates/server-deployment.yaml)
assert "the server Service names h2c as its application protocol" \
  grep -q 'appProtocol: kubernetes.io/h2c' "$TMP/render/ci.yaml"
assert "autoscaling leaves replicas out of the Deployment" \
  test "$(grep -c '^  replicas:' "$TMP/render/autoscaling.yaml")" = 0
assert "the minimal value set renders no web, ssh or ConfigMap object" \
  test "$(grep -cE '^kind: (ConfigMap|HorizontalPodAutoscaler)$|name: bee-(web|ssh)$' "$TMP/render/minimal.yaml")" = 0

expect_failure() { # description, expected message fragment, helm template arguments
  local what=$1 msg=$2
  shift 2
  if helm template bee "$CHART" "$@" >"$TMP/neg.out" 2>&1; then
    die "helm template accepted: $what"
  fi
  grep -q -- "$msg" "$TMP/neg.out" || { cat "$TMP/neg.out" >&2; die "helm template refused '$what' with the wrong message (wanted: $msg)"; }
  say "ok: refused: $what"
}
expect_failure "no config.publicUrl" "config.publicUrl is required" --set config.s3.bucket=b --set secrets.existingSecret=s
expect_failure "no secrets.existingSecret" "secrets.existingSecret is required" --set config.publicUrl=https://t.example --set config.s3.bucket=b
expect_failure "no config.s3.bucket" "config.s3.bucket is required" --set config.publicUrl=https://t.example --set secrets.existingSecret=s
expect_failure "a trailing slash in publicUrl (schema)" "does not match pattern" "${REQUIRED[@]}" --set config.publicUrl=https://t.example/
expect_failure "an unknown edition (schema)" "value must be one of 'enterprise', 'cloud'" "${REQUIRED[@]}" --set config.edition=hosted
expect_failure "zero replicas (schema)" "minimum: got 0, want 1" "${REQUIRED[@]}" --set server.replicas=0
expect_failure "an unknown top-level key (schema)" "additional properties" "${REQUIRED[@]}" --set replicaCount=3
expect_failure "ingress without a host" "ingress.host is required" "${REQUIRED[@]}" --set ingress.enabled=true
expect_failure "several replicas without the shared keys" "secrets.sharedKeys.enabled is false but the server can run 2 replicas" \
  "${REQUIRED[@]}" --set secrets.sharedKeys.enabled=false
expect_failure "autoscaling above one replica without the shared keys" "can run 10 replicas" \
  "${REQUIRED[@]}" --set secrets.sharedKeys.enabled=false --set server.replicas=1 --set server.autoscaling.enabled=true
assert "the shared keys reach server.toml and the environment" \
  grep -q 'oidc_key_pkcs8_hex = "$BEE_OIDC_KEY_HEX"' "$TMP/render/defaults.yaml"
assert "a single replica may run without them (no key in the rendering)" \
  test "$(grep -c 'BEE_URL_KEY_HEX\|BEE_OIDC_KEY_HEX' "$TMP/render/minimal.yaml")" = 0

# The kubeconfig guard: with KUBECONFIG empty or unset the wrappers must stop before any kubectl or
# helm process starts.
no_kubeconfig() ( unset KUBECONFIG; "$@" )
empty_kubeconfig() ( export KUBECONFIG=""; "$@" )
foreign_kubeconfig() ( export KUBECONFIG="/nonexistent/kubeconfig"; "$@" )
for guard in no_kubeconfig empty_kubeconfig foreign_kubeconfig; do
  for wrapper in kc hm; do
    if "$guard" "$wrapper" version >"$TMP/guard.out" 2>&1; then die "$wrapper ran with $guard"; fi
    grep -q 'SAFETY: ' "$TMP/guard.out" || { cat "$TMP/guard.out" >&2; die "$wrapper under $guard failed for another reason than the safety check"; }
  done
done
say "ok: kc and hm refuse to run with KUBECONFIG unset, empty or not the script's own file"

if [ -n "${BEE_HELM_STATIC_ONLY:-}" ]; then
  phase ""
  print_timings
  echo "$NAME: ok (static checks only: BEE_HELM_STATIC_ONLY is set)"
  exit 0
fi

# ---------------------------------------------------------------------------------------------
phase "tools: kind $KIND_VERSION"
# ---------------------------------------------------------------------------------------------
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) KIND_PLATFORM=darwin-arm64; KIND_SHA=0c8c7dbe5e23594a198b786c4bc13dacc101fa6196b0cb0b23a1ca44e61f4b4f ;;
  Darwin-x86_64) KIND_PLATFORM=darwin-amd64; KIND_SHA=5a99f26f57246dc9319dd294803313197a0f34d33c525b3ea8b655db5916ece0 ;;
  Linux-x86_64) KIND_PLATFORM=linux-amd64; KIND_SHA=aee6151561422756b764a4ae28e7f44cda5af5a9eead3cc9985112b1de8d8e0d ;;
  Linux-aarch64 | Linux-arm64) KIND_PLATFORM=linux-arm64; KIND_SHA=20022bee6cfcd5086cb7234d218e3454e6090022f2a8f55d1fa7fcf42c3867a2 ;;
  *) die "no pinned kind binary for $(uname -s)-$(uname -m)" ;;
esac
KIND="$TOOLS/kind-$KIND_VERSION-$KIND_PLATFORM"
if [ ! -x "$KIND" ] || [ "$(sha256_of "$KIND")" != "$KIND_SHA" ]; then
  mkdir -p "$TOOLS"
  curl -fsSL --retry 3 -o "$TMP/kind.download" "https://github.com/kubernetes-sigs/kind/releases/download/$KIND_VERSION/kind-$KIND_PLATFORM"
  [ "$(sha256_of "$TMP/kind.download")" = "$KIND_SHA" ] || die "the downloaded kind binary does not match the pinned sha256 $KIND_SHA"
  chmod +x "$TMP/kind.download"
  mv "$TMP/kind.download" "$KIND"
fi
[ "$(sha256_of "$KIND")" = "$KIND_SHA" ] || die "kind at $KIND does not match the pinned sha256"
say "ok: kind $KIND_VERSION ($KIND_PLATFORM) matches sha256 ${KIND_SHA:0:16}..."
INGRESS_MANIFEST="$TOOLS/ingress-nginx-$INGRESS_NGINX_VERSION.yaml"
if [ ! -f "$INGRESS_MANIFEST" ] || [ "$(sha256_of "$INGRESS_MANIFEST")" != "$INGRESS_NGINX_SHA" ]; then
  mkdir -p "$TOOLS"
  curl -fsSL --retry 3 -o "$TMP/ingress.download" \
    "https://raw.githubusercontent.com/kubernetes/ingress-nginx/$INGRESS_NGINX_VERSION/deploy/static/provider/kind/deploy.yaml"
  [ "$(sha256_of "$TMP/ingress.download")" = "$INGRESS_NGINX_SHA" ] || die "the downloaded ingress-nginx manifest does not match the pinned sha256 $INGRESS_NGINX_SHA"
  mv "$TMP/ingress.download" "$INGRESS_MANIFEST"
fi
say "ok: ingress-nginx $INGRESS_NGINX_VERSION manifest matches sha256 ${INGRESS_NGINX_SHA:0:16}..."
for p in $API_PORT $WEB_PORT $OIDC_PORT $INGRESS_HTTP_PORT $INGRESS_HTTPS_PORT; do
  ! port_busy "$p" || die "host port $p is already in use; stop whatever listens on it"
done

# ---------------------------------------------------------------------------------------------
phase "build images"
# ---------------------------------------------------------------------------------------------
if [ -n "${BEE_HELM_SKIP_BUILD:-}" ]; then
  # Keep images of a run before this one (bee-server:helm-ci); otherwise take the latest tags.
  docker image inspect "$SERVER_IMAGE" >/dev/null 2>&1 || docker tag bee-server:latest "$SERVER_IMAGE"
  docker image inspect "$WEB_IMAGE" >/dev/null 2>&1 || docker tag bee-web:latest "$WEB_IMAGE"
  say "BEE_HELM_SKIP_BUILD is set: reusing the images already tagged $SERVER_IMAGE and $WEB_IMAGE"
else
  SHA="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
  locked docker build -q -t "$SERVER_IMAGE" --build-arg "CARGO_BUILD_JOBS=$BUILD_JOBS" --build-arg "BUILD_SHA=$SHA" . >/dev/null
  locked docker build -q -t "$WEB_IMAGE" --build-arg "BUILD_SHA=$SHA" -f apps/web/Dockerfile . >/dev/null
fi
docker image ls --format '{{.Repository}}:{{.Tag}} {{.Size}}' | grep -E "^(bee-server:helm-ci|bee-web:helm-ci) " | sed "s/^/$NAME: image /"

# ---------------------------------------------------------------------------------------------
phase "kind cluster"
# ---------------------------------------------------------------------------------------------
# A cluster left over from an interrupted run has this name; kind owns it by name only.
require_kubeconfig
"$KIND" delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
"$KIND" create cluster --name "$CLUSTER" --image "$KIND_NODE_IMAGE" --kubeconfig "$KUBECONFIG" --wait 180s >"$TMP/kind.out" 2>&1 \
  || { cat "$TMP/kind.out" >&2; die "kind could not create the cluster"; }
CLUSTER_CREATED=1
assert_own_cluster
say "ok: current context is $(kubectl --kubeconfig "$KUBECONFIG" config current-context) ($(kubectl --kubeconfig "$KUBECONFIG" config view --minify -o 'jsonpath={.clusters[0].cluster.server}'))"
assert "the kubeconfig holds exactly one context" \
  test "$(kubectl --kubeconfig "$KUBECONFIG" config get-contexts -o name | wc -l | tr -d ' ')" = 1
say "node: $(kc get nodes -o 'jsonpath={.items[0].status.nodeInfo.kubeletVersion} {.items[0].status.nodeInfo.architecture}')"
for img in "$SERVER_IMAGE" "$WEB_IMAGE"; do
  "$KIND" load docker-image "$img" --name "$CLUSTER" >"$TMP/load.out" 2>&1 \
    || { cat "$TMP/load.out" >&2; die "kind could not load $img"; }
  say "ok: loaded $img into the cluster"
done

# ---------------------------------------------------------------------------------------------
phase "dependencies and secrets"
# ---------------------------------------------------------------------------------------------
kc create namespace "$NS" >/dev/null
PG_PASSWORD="$(rand 16)"
S3_ACCESS_KEY="ak$(rand 8)"
S3_SECRET_KEY="sk$(rand 16)"
SIGNING_KEY="$(rand 32)"
URL_KEY="$(rand 32)"
# The OpenID provider's P-256 key as the hex of its PKCS#8 DER (the replicas share it).
OIDC_KEY="$(openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 2>/dev/null | openssl pkcs8 -topk8 -nocrypt -outform DER 2>/dev/null | od -An -v -tx1 | tr -d ' \n')"
[ -n "$OIDC_KEY" ] || die "openssl could not generate a P-256 key"
kc -n "$NS" create secret generic ci-credentials \
  --from-literal="postgres-password=$PG_PASSWORD" --from-literal="s3-access-key=$S3_ACCESS_KEY" \
  --from-literal="s3-secret-key=$S3_SECRET_KEY" >/dev/null
kc -n "$NS" create secret generic bee-secrets \
  --from-literal="postgres-url=postgres://bee:$PG_PASSWORD@postgres:5432/bee" \
  --from-literal="forge-database=postgres://bee:$PG_PASSWORD@postgres:5432/bee?schema=bee_forge" \
  --from-literal="s3-access-key=$S3_ACCESS_KEY" --from-literal="s3-secret-key=$S3_SECRET_KEY" \
  --from-literal="signing-key-hex=$SIGNING_KEY" \
  --from-literal="url-key-hex=$URL_KEY" --from-literal="oidc-key-pkcs8-hex=$OIDC_KEY" >/dev/null
ssh-keygen -q -t ed25519 -N '' -C '' -f "$TMP/ssh_host_ed25519_key"
kc -n "$NS" create secret generic bee-ssh --from-file="ssh_host_ed25519_key=$TMP/ssh_host_ed25519_key" >/dev/null
kc -n "$NS" apply -f "$CI_DIR/dependencies.yaml" >/dev/null
for d in postgres rustfs mock-oidc; do
  kc -n "$NS" rollout status "deploy/$d" --timeout="${TIMEOUT_SECS}s" >/dev/null
  say "ok: $d is ready"
done
kc -n "$NS" wait --for=condition=complete job/s3-init --timeout="${TIMEOUT_SECS}s" >/dev/null
say "ok: the s3-init job created the bucket"

# ---------------------------------------------------------------------------------------------
phase "helm install"
# ---------------------------------------------------------------------------------------------
SETS=()
install_or_upgrade_quiet() { # arguments: extra --set flags go into SETS before the call; returns helm's status
  hm upgrade --install "$RELEASE" "$CHART" -n "$NS" -f "$CI_DIR/values.yaml" ${SETS[@]+"${SETS[@]}"} \
    --wait --timeout "${TIMEOUT_SECS}s" >"$TMP/helm.out" 2>&1 || return 1
  # helm --wait returns once the new pods are ready, with an old pod possibly not yet told to stop;
  # rollout status returns when the old ReplicaSet is scaled down.
  for d in server web; do
    kc -n "$NS" rollout status "deploy/$RELEASE-$d" --timeout="${TIMEOUT_SECS}s" >>"$TMP/helm.out" 2>&1 || return 1
  done
}
install_or_upgrade() {
  install_or_upgrade_quiet || { cat "$TMP/helm.out" >&2; die "helm upgrade --install failed"; }
}
install_or_upgrade
say "release: $(hm -n "$NS" list -o json | jq -r '.[0] | "\(.name) revision \(.revision) status \(.status) chart \(.chart)"')"
assert "2 server replicas are ready after the install" \
  test "$(kc -n "$NS" get deploy "$RELEASE-server" -o 'jsonpath={.status.readyReplicas}')" = 2
assert "2 web replicas are ready after the install" \
  test "$(kc -n "$NS" get deploy "$RELEASE-web" -o 'jsonpath={.status.readyReplicas}')" = 2
assert "the check-config init container ran and succeeded in every server pod" \
  test "$(kc -n "$NS" get pods -l app.kubernetes.io/component=server -o json \
    | jq '[.items[] | select(.status.initContainerStatuses[0].name == "check-config" and .status.initContainerStatuses[0].state.terminated.exitCode == 0)] | length')" = 2
# Helm 4.2.1 waits for the test pod's deletion until --timeout even when the pod is long gone
# (measured: `helm test` of a busybox echo pod took 302 s with the default 5 m, 18 s with
# --timeout 15s), once before creating the hook and once after it succeeds. The test pod itself
# needs a few seconds, so a short timeout costs nothing but that wait.
T0=$SECONDS
hm -n "$NS" test "$RELEASE" --logs --timeout 40s >"$TMP/helm-test.out" 2>&1 || { cat "$TMP/helm-test.out" >&2; die "helm test failed"; }
assert "helm test: the test pod saw the server answering through its Service" grep -q 'server answers' "$TMP/helm-test.out"
assert "helm test: the test pod saw the web app answering through its Service" grep -q 'web answers' "$TMP/helm-test.out"
say "helm test took $((SECONDS - T0))s"

# A client pod in the cluster: the bee CLI, git and curl from the server image, talking to
# the Service the way an agent in the cluster would.
kc -n "$NS" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: bee-client
spec:
  automountServiceAccountToken: false
  securityContext:
    runAsNonRoot: true
    runAsUser: 10001
    runAsGroup: 10001
  containers:
    - name: client
      image: $SERVER_IMAGE
      imagePullPolicy: Never
      command: ["sleep", "infinity"]
      env:
        - name: HOME
          value: /tmp/home
        - name: BEE_ENDPOINT
          value: http://$RELEASE-server:7420
      volumeMounts:
        - name: tmp
          mountPath: /tmp
  volumes:
    - name: tmp
      emptyDir: {}
EOF
kc -n "$NS" wait --for=condition=Ready pod/bee-client --timeout="${TIMEOUT_SECS}s" >/dev/null
kc -n "$NS" exec bee-client -- mkdir -p /tmp/home /tmp/work

# ---------------------------------------------------------------------------------------------
# helpers that talk to the cluster
# ---------------------------------------------------------------------------------------------
pf_start() { # name target local-port remote-port [namespace]: (re)starts a port-forward and waits for it
  pf_stop "$1"
  assert_own_cluster
  # kubectl itself runs in the background (not the kc function: killing a function's subshell would
  # leave kubectl running, holding the port).
  kubectl --kubeconfig "$KUBECONFIG" --context "$CTX" -n "${5:-$NS}" port-forward "$2" "$3:$4" >"$TMP/pf-$1.log" 2>&1 &
  local pid=$! waited=0
  echo "$pid" >"$TMP/pf-$1.pid"
  # The port must be open and held by this forward, not by a predecessor that is still dying.
  until port_busy "$3" && kill -0 "$pid" 2>/dev/null; do
    [ "$waited" -lt 60 ] || { cat "$TMP/pf-$1.log" >&2; die "port-forward $1 did not open port $3"; }
    sleep 1
    waited=$((waited + 1))
  done
}
ready_pod() { # component: the name of a Ready pod that is not terminating (old pods linger after a rollout)
  kc -n "$NS" get pods -l "app.kubernetes.io/component=$1" -o json | jq -r \
    '[.items[] | select(.metadata.deletionTimestamp == null and any(.status.conditions[]?; .type == "Ready" and .status == "True"))][0].metadata.name // empty'
}
forward_all() {
  local server web waited=0
  until server="$(ready_pod server)" && web="$(ready_pod web)" && [ -n "$server" ] && [ -n "$web" ]; do
    [ "$waited" -lt 60 ] || die "no ready server and web pod to forward to"
    sleep 1
    waited=$((waited + 1))
  done
  pf_start server "pod/$server" "$API_PORT" 7420
  pf_start web "pod/$web" "$WEB_PORT" 3100
  pf_start oidc svc/mock-oidc "$OIDC_PORT" 8080
  wait_for_code "server /readyz" "$API/readyz" 200
}

rpc() { # <Service/Method> <json> [token]
  local method=$1 body=$2 token=${3-${TOKEN:-}} out code
  out="$TMP/rpc.out"
  local auth=()
  [ -z "$token" ] || auth=(-H "Authorization: Bearer $token")
  code="$(curl -sS --max-time 60 -o "$out" -w '%{http_code}' -X POST "$API/bee.v1.$method" \
    -H 'Content-Type: application/json' ${auth[@]+"${auth[@]}"} -d "$body")" \
    || die "$method: the request failed"
  [ "$code" = "200" ] || die "$method answered HTTP $code: $(cat "$out")"
  cat "$out"
}

oidc_id_token() {
  local q="client_id=$OIDC_CLIENT_ID&response_type=code&redirect_uri=http://localhost/callback&scope=openid&state=s"
  local base="http://127.0.0.1:$OIDC_PORT/default" location code
  location="$(curl -sS -i -H "Host: $OIDC_HOST" -X POST -d 'username=admin&claims=' "$base/authorize?$q" \
    | awk 'tolower($1) == "location:" { print $2 }' | tr -d '\r')"
  code="${location#*code=}"
  code="${code%%&*}"
  if [ -z "$code" ] || [ "$code" = "$location" ]; then
    die "the mock OIDC provider issued no code"
  fi
  curl -sS -H "Host: $OIDC_HOST" -X POST \
    -d "grant_type=authorization_code&code=$code&client_id=$OIDC_CLIENT_ID&client_secret=unused&redirect_uri=http://localhost/callback" \
    "$base/token" | jq -er .id_token
}
oidc_login() {
  local jwt
  jwt="$(oidc_id_token)"
  rpc AuthService/Login "$(jq -nc --arg t "$jwt" '{oidcIdToken: $t}')" "" | jq -er .token
}

bee() { kc -n "$NS" exec -i bee-client -- env "BEE_TOKEN=$TOKEN" bee "$@"; }

GITHOME="$TMP/githome"
mkdir -p "$GITHOME"
g() {
  HOME="$GITHOME" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 \
    git -c "http.extraHeader=Authorization: Bearer $TOKEN" "$@"
}

REPO_REF='{"fullName":"acme/app"}'
main_ref() { rpc RefService/ListRefs "{\"repo\":$REPO_REF}" | jq -er '.refs[] | select(.name == "main") | "\(.version) \(.target.raw)"'; }
log_json() { rpc QueryService/Log "{\"repo\":$REPO_REF,\"from\":{\"expr\":\"main\"},\"includeMeta\":true}"; }

server_pods() { kc -n "$NS" get pods -l app.kubernetes.io/component=server --field-selector=status.phase=Running -o 'jsonpath={range .items[*]}{.metadata.name} {.status.podIP}{"\n"}{end}'; }

# ---------------------------------------------------------------------------------------------
phase "first admin"
# ---------------------------------------------------------------------------------------------
forward_all
INFO="$(rpc ObjectService/GetServerInfo '{}' "")"
assert "the server reports the enterprise edition" test "$(jq -r .edition <<<"$INFO")" = enterprise
TOKEN="$(oidc_login)"
ADMIN_ID="$(rpc AuthService/WhoAmI '{}' | jq -er .principal.id)"
say "first sign-in made principal $ADMIN_ID"
assert "a new sign-in is not yet an operator (CreateBackup is refused)" \
  test "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/bee.v1.AdminService/CreateBackup" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $TOKEN" -d '{}')" != 200
SETS+=(--set "config.siteAdmins={$ADMIN_ID}")
install_or_upgrade
say "ok: upgraded with the first site admin (revision $(hm -n "$NS" list -o json | jq -r '.[0].revision'))"
forward_all
TOKEN="$(oidc_login)"
assert "the same principal signed in again" test "$(rpc AuthService/WhoAmI '{}' | jq -er .principal.id)" = "$ADMIN_ID"

# ---------------------------------------------------------------------------------------------
phase "org, repo, land with the CLI"
# ---------------------------------------------------------------------------------------------
rpc OrgService/CreateOrg '{"name":"acme","display":"Acme"}' >/dev/null
REPO="$(rpc RepoService/CreateRepo '{"org":"acme","name":"app","visibility":"VISIBILITY_PUBLIC","description":"Helm check","autoInit":true}')"
assert "the repo is acme/app" test "$(jq -r .fullName <<<"$REPO")" = acme/app
bee clone acme/app /tmp/work/wc >/dev/null
kc -n "$NS" exec bee-client -- sh -c "printf 'hello from the bee CLI\nsecond line\n' > /tmp/work/wc/hello.txt"
bee -C /tmp/work/wc commit -m "Add hello.txt" --json >/dev/null
LAND="$(bee -C /tmp/work/wc land --json)"
assert "the CLI landed one change on main" test "$(jq -r '.data.land.landed | length' <<<"$LAND")" = 1
assert "bee cat reads the landed file" test "$(bee cat --repo acme/app hello.txt)" = "$(printf 'hello from the bee CLI\nsecond line')"
read -r V_AFTER_CLI _ <<<"$(main_ref)"

# ---------------------------------------------------------------------------------------------
phase "git clone, push, ssh, web"
# ---------------------------------------------------------------------------------------------
g clone -q "$API/acme/app.git" "$TMP/gitwc"
assert "git clone has README.md and hello.txt" test -f "$TMP/gitwc/README.md" -a -f "$TMP/gitwc/hello.txt"
echo "pushed with git" >"$TMP/gitwc/from-git.txt"
g -C "$TMP/gitwc" add from-git.txt
g -C "$TMP/gitwc" -c user.name="Helm Check" -c user.email=helm@example.com commit -q -m "Add from-git.txt"
g -C "$TMP/gitwc" push -q origin HEAD:main
read -r V_AFTER_PUSH _ <<<"$(main_ref)"
assert "the push advanced main (version $V_AFTER_CLI -> $V_AFTER_PUSH)" test "$V_AFTER_PUSH" -gt "$V_AFTER_CLI"
assert "bee cat reads the pushed file" test "$(bee cat --repo acme/app from-git.txt)" = "pushed with git"
assert "git over SSH answers through the ssh Service" \
  test "$(kc -n "$NS" exec bee-client -- bash -c "exec 3<>/dev/tcp/$RELEASE-ssh/2222; head -c 4 <&3")" = "SSH-"
page() { curl -sL --max-time 30 -o "$TMP/page.html" -w '%{http_code}' "$@"; }
assert "web home answers 200" test "$(page "$WEB/")" = 200
assert "the public repo page answers 200 and names acme/app" \
  test "$(page "$WEB/acme/app")" = 200 -a "$(grep -c 'acme/app' "$TMP/page.html")" -ge 1
assert "signed in (session cookie), the admin console answers 200" \
  test "$(page -b "bee_token=$TOKEN" "$WEB/admin")" = 200 -a "$(grep -c '<title>Admin' "$TMP/page.html")" -ge 1

# ---------------------------------------------------------------------------------------------
phase "pod hardening"
# ---------------------------------------------------------------------------------------------
# `sed -n 1p`, not `head -1`: head exits after one line, and under pipefail a kubectl that is
# still writing the second pod's line then fails the pipeline with SIGPIPE (exit 141).
SERVER_POD="$(server_pods | sed -n 1p | cut -d' ' -f1)"
WEB_POD="$(kc -n "$NS" get pods -l app.kubernetes.io/component=web -o 'jsonpath={.items[0].metadata.name}')"
assert "the server runs as uid 10001" test "$(kc -n "$NS" exec "$SERVER_POD" -c server -- id -u)" = 10001
assert "the web app runs as uid 1000" test "$(kc -n "$NS" exec "$WEB_POD" -c web -- id -u)" = 1000
refute "the server's root filesystem is read-only" kc -n "$NS" exec "$SERVER_POD" -c server -- touch /usr/local/bin/x
refute "the web app's root filesystem is read-only" kc -n "$NS" exec "$WEB_POD" -c web -- touch /app/x
assert "the server can write /data and /tmp" kc -n "$NS" exec "$SERVER_POD" -c server -- sh -c 'touch /data/.w /tmp/.w'
assert "the pods mount no ServiceAccount token" \
  test "$(kc -n "$NS" get pod "$SERVER_POD" -o 'jsonpath={.spec.automountServiceAccountToken}')" = false
assert "the SSH host key is mounted from the Secret" kc -n "$NS" exec "$SERVER_POD" -c server -- test -f /etc/bee-ssh/ssh_host_ed25519_key
refute "the server generated no SSH host key of its own" kc -n "$NS" exec "$SERVER_POD" -c server -- test -e /data/ssh_host_ed25519_key
assert "every pod mounts the same SSH host key" \
  test "$(for pod in $(server_pods | cut -d' ' -f1); do kc -n "$NS" exec "$pod" -c server -- sha256sum /etc/bee-ssh/ssh_host_ed25519_key; done | sort -u | wc -l | tr -d ' ')" = 1
assert "both PodDisruptionBudgets exist" test "$(kc -n "$NS" get pdb -o name | wc -l | tr -d ' ')" = 2

# ---------------------------------------------------------------------------------------------
phase "helm upgrade to 3 replicas"
# ---------------------------------------------------------------------------------------------
SETS+=(--set server.replicas=3)
install_or_upgrade
say "release: $(hm -n "$NS" list -o json | jq -r '.[0] | "\(.name) revision \(.revision) status \(.status)"')"
assert "3 server replicas are ready" test "$(kc -n "$NS" get deploy "$RELEASE-server" -o 'jsonpath={.status.readyReplicas}')" = 3
assert "the 3 replicas are 3 distinct pods with 3 distinct IPs" test "$(server_pods | cut -d' ' -f2 | sort -u | wc -l | tr -d ' ')" = 3
forward_all
TOKEN="$(oidc_login)"

# ---------------------------------------------------------------------------------------------
phase "20 concurrent lands through the Service"
# ---------------------------------------------------------------------------------------------
LANDS=20
read -r V_BEFORE _ <<<"$(main_ref)"
LAND_MS0="$(python3 -c 'import time; print(int(time.time()*1000))')"
kc -n "$NS" exec -i bee-client -- env "BEE_TOKEN=$TOKEN" sh -s >"$TMP/lands.json" 2>"$TMP/lands.err" <<EOS || { cat "$TMP/lands.err" >&2; die "a concurrent land failed"; }
set -eu
cd /tmp/work
seq 1 $LANDS | xargs -P $LANDS -I{} sh -c '
  set -eu
  bee clone acme/app c{} >/dev/null
  echo "concurrent {}" > c{}/concurrent-{}.txt
  bee -C c{} commit -m "Concurrent land {}" --json >/dev/null
  bee -C c{} land --json > land-{}.json
'
cat land-*.json
EOS
LAND_MS1="$(python3 -c 'import time; print(int(time.time()*1000))')"
LAND_ELAPSED_MS=$((LAND_MS1 - LAND_MS0))
assert "each of the $LANDS lands reported exactly one landed change" \
  jq -s -e --argjson n "$LANDS" 'length == $n and all(.[]; (.data.land.landed | length) == 1)' "$TMP/lands.json"
say "ok: $LANDS lands finished in ${LAND_ELAPSED_MS} ms (each: clone, commit, land; $(uname -s) $(uname -m), load average $(uptime | sed 's/.*load average[s]*: //'))"
read -r V_AFTER _ <<<"$(main_ref)"
say "main moved from version $V_BEFORE to $V_AFTER"
LOG="$(log_json)"
for i in $(seq 1 $LANDS); do
  n="$(jq --arg m "Concurrent land $i" '[.changesets[] | select(.meta.message == $m)] | length' <<<"$LOG")"
  [ "$n" = 1 ] || die "the change 'Concurrent land $i' is in main's history $n times, not once"
done
say "ok: all $LANDS concurrent changes are in main's history exactly once"
assert "history is linear: every changeset on main has one parent" \
  jq -e '[.changesets[] | select((.parents | length) > 1)] | length == 0' <<<"$LOG"
g -C "$TMP/gitwc" pull -q --ff-only origin main
FILES="$(find "$TMP/gitwc" -path "$TMP/gitwc/.git" -prune -o -type f \( -name 'concurrent-*.txt' -o -name hello.txt \) -print | wc -l | tr -d ' ')"
assert "all 21 files (hello.txt and 20 concurrent files) are at the tip of main" test "$FILES" = 21
for i in $(seq 1 $LANDS); do
  [ "$(cat "$TMP/gitwc/concurrent-$i.txt")" = "concurrent $i" ] || die "concurrent-$i.txt has the wrong content"
done
say "ok: every concurrent file has its own content"
TIP="$(main_ref)"
while read -r pod ip; do
  seen="$(kc -n "$NS" exec bee-client -- curl -fsS --max-time 20 -X POST "http://$ip:7420/bee.v1.RefService/ListRefs" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $TOKEN" -d "{\"repo\":$REPO_REF}" \
    | jq -er '.refs[] | select(.name == "main") | "\(.version) \(.target.raw)"')"
  [ "$seen" = "$TIP" ] || die "replica $pod ($ip) reports main as '$seen', the Service reports '$TIP'"
  say "ok: replica $pod ($ip) reports main at $seen"
done < <(server_pods)

# The OpenID provider's key set. Every pod must publish the same one, because Actions OIDC tokens and
# attestations signed by one pod are verified against whichever pod serves the JWKS (a pod with its
# own key publishes a set that does not verify the others' signatures).
JWKS_SERVICE="$(kc -n "$NS" exec bee-client -- curl -fsS --max-time 20 "http://$RELEASE-server:7420/.well-known/jwks.json" | jq -S .)"
assert "the Service serves a JWKS with at least one key" jq -e '(.keys | length) >= 1' <<<"$JWKS_SERVICE"
while read -r pod ip; do
  pod_jwks="$(kc -n "$NS" exec bee-client -- curl -fsS --max-time 20 "http://$ip:7420/.well-known/jwks.json" | jq -S .)"
  [ "$pod_jwks" = "$JWKS_SERVICE" ] || die "replica $pod ($ip) serves a JWKS that differs from the Service's: $pod_jwks"
  say "ok: replica $pod ($ip) serves the same JWKS as the Service ($(jq -r '[.keys[].kid] | join(",")' <<<"$pod_jwks"))"
done < <(server_pods)

# ---------------------------------------------------------------------------------------------
phase "pod termination and rolling restart"
# ---------------------------------------------------------------------------------------------
VICTIM="$(server_pods | sed -n 1p | cut -d' ' -f1)"
T0=$SECONDS
kc -n "$NS" delete pod "$VICTIM" --wait=true >/dev/null
STOP_SECS=$((SECONDS - T0))
say "deleting pod $VICTIM took ${STOP_SECS}s (preStop sleep 5 s, terminationGracePeriodSeconds 60)"
assert "a server pod stops on SIGTERM well inside its 60 s grace period (not killed by SIGKILL)" test "$STOP_SECS" -lt 40
kc -n "$NS" rollout status "deploy/$RELEASE-server" --timeout="${TIMEOUT_SECS}s" >/dev/null
assert "the Deployment replaced the pod: 3 ready replicas" test "$(kc -n "$NS" get deploy "$RELEASE-server" -o 'jsonpath={.status.readyReplicas}')" = 3

# Probes go through the Service for the whole rollout; the Deployment never removes a ready pod
# before its replacement is ready (maxUnavailable 0) and the preStop sleep covers endpoint removal.
kc -n "$NS" exec bee-client -- rm -f /tmp/probe-stop /tmp/probe.out
kc -n "$NS" exec bee-client -- sh -c "
  : > /tmp/probe.out
  n=0; f=0
  while [ ! -e /tmp/probe-stop ]; do
    code=\$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 http://$RELEASE-server:7420/readyz || true)
    n=\$((n + 1)); [ \"\$code\" = 200 ] || f=\$((f + 1))
    sleep 0.1
  done
  echo \"\$n \$f\" > /tmp/probe.out
" >/dev/null 2>&1 &
PROBE_PID=$!
sleep 2
T0=$SECONDS
kc -n "$NS" rollout restart "deploy/$RELEASE-server" >/dev/null
kc -n "$NS" rollout status "deploy/$RELEASE-server" --timeout="${TIMEOUT_SECS}s" >/dev/null
ROLL_SECS=$((SECONDS - T0))
sleep 2
kc -n "$NS" exec bee-client -- touch /tmp/probe-stop
wait "$PROBE_PID" || true
read -r PROBES PROBE_FAILURES <<<"$(kc -n "$NS" exec bee-client -- cat /tmp/probe.out)"
say "rolling restart of 3 replicas took ${ROLL_SECS}s; $PROBES /readyz probes through the Service, $PROBE_FAILURES failed"
assert "the rolling restart dropped no probe ($PROBES sent)" test "$PROBES" -gt 50 -a "$PROBE_FAILURES" = 0
forward_all
TOKEN="$(oidc_login)"
assert "the same tip survived the restarts" test "$(main_ref)" = "$TIP"
LOG="$(log_json)"
MESSAGES="$(for i in $(seq 1 $LANDS); do echo "Concurrent land $i"; done; echo "Initial commit"; echo "Add hello.txt"; echo "Add from-git.txt")"
assert "every change is in main's history exactly once after the restarts (initial commit, hello.txt, from-git.txt, 20 concurrent; $(jq '.changesets | length' <<<"$LOG") log entries)" \
  jq -e --arg want "$MESSAGES" '[.changesets[].meta.message // ""] as $got | ($want | split("\n")) | all(. as $m | ([$got[] | select(. == $m or startswith($m + "\n"))] | length) == 1)' <<<"$LOG"
g -C "$TMP/gitwc" pull -q --ff-only origin main
bee clone acme/app /tmp/work/after >/dev/null
kc -n "$NS" exec bee-client -- sh -c "echo 'after the restarts' > /tmp/work/after/after.txt"
bee -C /tmp/work/after commit -m "Add after.txt" --json >/dev/null
assert "the restarted replicas take a new land" \
  test "$(bee -C /tmp/work/after land --json | jq -r '.data.land.landed | length')" = 1

# ---------------------------------------------------------------------------------------------
phase "a pod without its forge is not ready"
# ---------------------------------------------------------------------------------------------
# The forge's database URL is pointed at a host that does not exist and the Deployment is restarted:
# the new pod's forge cannot open, the server serves without it, and /readyz must say so (503) while
# /healthz stays 200. With maxUnavailable 0 the old pods keep serving and the rollout stalls.
GOOD_FORGE_B64="$(kc -n "$NS" get secret bee-secrets -o 'jsonpath={.data.forge-database}')"
BAD_FORGE_B64="$(printf %s 'postgres://bee:x@no-such-host.invalid:5432/bee?schema=bee_forge' | base64 | tr -d '\n')"
PODS_BEFORE="$(server_pods | cut -d' ' -f1 | sort)"
kc -n "$NS" patch secret bee-secrets -p "{\"data\":{\"forge-database\":\"$BAD_FORGE_B64\"}}" >/dev/null
kc -n "$NS" rollout restart "deploy/$RELEASE-server" >/dev/null
BAD_POD="" BAD_IP=""
waited=0
until [ -n "$BAD_POD" ]; do
  [ "$waited" -lt 120 ] || die "no new server pod appeared after the restart"
  while read -r pod ip; do
    grep -qx "$pod" <<<"$PODS_BEFORE" || { BAD_POD="$pod"; BAD_IP="$ip"; }
  done < <(server_pods)
  [ -n "$BAD_POD" ] || { sleep 2; waited=$((waited + 2)); }
done
waited=0
until kc -n "$NS" exec bee-client -- curl -fsS --max-time 3 "http://$BAD_IP:7420/healthz" >/dev/null 2>&1; do
  [ "$waited" -lt 60 ] || die "the new pod $BAD_POD never answered /healthz"
  sleep 1
  waited=$((waited + 1))
done
assert "the pod without its forge answers /healthz 200 (it is alive)" \
  test "$(kc -n "$NS" exec bee-client -- curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://$BAD_IP:7420/healthz")" = 200
assert "the pod without its forge answers /readyz 503 saying why" \
  test "$(kc -n "$NS" exec bee-client -- curl -s -w ' %{http_code}' --max-time 5 "http://$BAD_IP:7420/readyz" | tr -d '\n')" = "forge unavailable: it is enabled but did not open 503"
sleep 12 # longer than the readiness probe's period and failure threshold
assert "the pod without its forge stays NotReady (the readiness probe sees the 503)" \
  test "$(kc -n "$NS" get pod "$BAD_POD" -o 'jsonpath={.status.containerStatuses[?(@.name=="server")].ready}')" = false
assert "the pod without its forge is not restarted for it (liveness uses /healthz)" \
  test "$(kc -n "$NS" get pod "$BAD_POD" -o 'jsonpath={.status.containerStatuses[?(@.name=="server")].restartCount}')" = 0
assert "the Service still answers /readyz 200 (the old pods keep serving)" \
  test "$(kc -n "$NS" exec bee-client -- curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://$RELEASE-server:7420/readyz")" = 200
refute "the rollout stalls instead of replacing working pods with it" kc -n "$NS" rollout status "deploy/$RELEASE-server" --timeout=10s
assert "the old replicas are all still ready" \
  test "$(kc -n "$NS" get deploy "$RELEASE-server" -o 'jsonpath={.status.readyReplicas}')" = 3
# Put the URL back and delete the stuck pod: its replacement opens the forge and the rollout finishes.
kc -n "$NS" patch secret bee-secrets -p "{\"data\":{\"forge-database\":\"$GOOD_FORGE_B64\"}}" >/dev/null
kc -n "$NS" delete pod "$BAD_POD" --wait=false >/dev/null
kc -n "$NS" rollout status "deploy/$RELEASE-server" --timeout="${TIMEOUT_SECS}s" >/dev/null
assert "after the URL is restored the rollout finishes with 3 ready replicas" \
  test "$(kc -n "$NS" get deploy "$RELEASE-server" -o 'jsonpath={.status.readyReplicas}')" = 3
forward_all
TOKEN="$(oidc_login)"
assert "the forge answers again (OrgService/GetOrg)" \
  test "$(rpc OrgService/GetOrg '{"nameOrId":"acme"}' | jq -r .name)" = acme

# ---------------------------------------------------------------------------------------------
phase "ingress-nginx routing"
# ---------------------------------------------------------------------------------------------
# The chart's Ingress with its default (regular expression) paths behind a real ingress-nginx:
# the API, git smart HTTP and the web app on one host, and a gRPC host on its own.
# The node cannot pull registry.k8s.io in reasonable time on every machine (it sat at ContainerCreating
# for more than 5 minutes in two runs), so the images are pulled on the host by their pinned digest and
# loaded into the node. `kind load docker-image` fails on multi-platform images ("content digest not
# found"), so each is saved for the node's platform and loaded as an archive; the loaded image has a
# tag but not the manifest-list digest, so the manifest is applied with its tag-only references.
NODE_ARCH="$(kc get nodes -o 'jsonpath={.items[0].status.nodeInfo.architecture}')"
INGRESS_APPLY="$INGRESS_MANIFEST"
while IFS= read -r img; do
  tagged="${img%%@*}"
  docker pull -q "$img" >/dev/null || die "could not pull $img on the host"
  docker tag "$img" "$tagged"
  docker save --platform "linux/$NODE_ARCH" -o "$TMP/ingress-image.tar" "$tagged" || die "could not save $tagged for linux/$NODE_ARCH"
  "$KIND" load image-archive "$TMP/ingress-image.tar" --name "$CLUSTER" >/dev/null 2>&1 || die "kind could not load $tagged"
  rm -f "$TMP/ingress-image.tar"
  say "ok: preloaded $tagged (pulled by digest ${img##*@sha256:})"
done < <(grep -o 'registry.k8s.io/[^[:space:]]*' "$INGRESS_MANIFEST" | sort -u)
sed -E 's#(registry\.k8s\.io/[^:@[:space:]]+:[^@[:space:]]+)@sha256:[0-9a-f]+#\1#' "$INGRESS_MANIFEST" >"$TMP/ingress-nginx.yaml"
INGRESS_APPLY="$TMP/ingress-nginx.yaml"
kc apply -f "$INGRESS_APPLY" >/dev/null
kc -n ingress-nginx wait --for=condition=complete job/ingress-nginx-admission-create --timeout="${TIMEOUT_SECS}s" >/dev/null
kc -n ingress-nginx rollout status deploy/ingress-nginx-controller --timeout="${TIMEOUT_SECS}s" >/dev/null
say "ok: ingress-nginx $INGRESS_NGINX_VERSION is ready"
SETS+=(--set ingress.enabled=true --set ingress.host=bee.test --set ingress.grpc.enabled=true --set ingress.grpc.host=grpc.bee.test)
waited=0
until install_or_upgrade_quiet; do # the admission webhook may need a moment after the controller is ready
  [ "$waited" -lt 60 ] || { cat "$TMP/helm.out" >&2; die "helm upgrade with the Ingress failed"; }
  sleep 3
  waited=$((waited + 3))
done
assert "the chart's two Ingresses exist" test "$(kc -n "$NS" get ingress -o name | wc -l | tr -d ' ')" = 2
pf_start ingress svc/ingress-nginx-controller "$INGRESS_HTTP_PORT" 80 ingress-nginx
pf_start ingress-tls svc/ingress-nginx-controller "$INGRESS_HTTPS_PORT" 443 ingress-nginx
via() { curl -sS --max-time 30 -H 'Host: bee.test' "$@"; }
IN="http://127.0.0.1:$INGRESS_HTTP_PORT"
waited=0
# Until the Ingress is loaded, ingress-nginx's own default server answers /healthz with 200 and an
# empty body; the server's body is "ok".
until [ "$(via "$IN/healthz" || true)" = ok ]; do
  [ "$waited" -lt 90 ] || die "ingress-nginx did not route /healthz to the server within 90s"
  sleep 2
  waited=$((waited + 2))
done
assert "/healthz goes to the server" test "$(via "$IN/healthz")" = ok
assert "an RPC goes to the server (WhoAmI returns the admin principal)" \
  test "$(via -X POST "$IN/bee.v1.AuthService/WhoAmI" -H 'Content-Type: application/json' -H "Authorization: Bearer $TOKEN" -d '{}' | jq -r .principal.id)" = "$ADMIN_ID"
assert "git smart HTTP goes to the server (ls-remote through the ingress)" \
  test "$(HOME="$GITHOME" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 \
    git -c "http.extraHeader=Authorization: Bearer $TOKEN" -c 'http.extraHeader=Host: bee.test' ls-remote "$IN/acme/app.git" HEAD | wc -l | tr -d ' ')" = 1
assert "the web home goes to the web app" test "$(via -o "$TMP/page.html" -w '%{http_code}' "$IN/")" = 200 -a "$(grep -c '<html' "$TMP/page.html")" -ge 1
assert "the repo page goes to the web app" \
  test "$(via -o "$TMP/page.html" -w '%{http_code}' "$IN/acme/app")" = 200 -a "$(grep -c 'acme/app' "$TMP/page.html")" -ge 1
assert "/metrics is not routed to the server" test "$(via "$IN/metrics" | grep -c '^# HELP')" = 0
# gRPC: a real gRPC call (an empty WhoAmI request, one 5-byte message frame) over HTTP/2 and TLS to
# the gRPC host. Without a token the server answers grpc-status 16 (UNAUTHENTICATED) in the
# response headers, which only a gRPC-speaking backend produces.
# The controller can answer 502 for a moment after it first learns the gRPC backend, so ask until
# a gRPC status comes back (ten tries at most); the assertion below is unchanged.
GRPC_HEADERS=""
for _ in $(seq 1 10); do
  GRPC_HEADERS="$(printf '\0\0\0\0\0' | curl -sSk --max-time 30 --http2 --resolve "grpc.bee.test:$INGRESS_HTTPS_PORT:127.0.0.1" \
    -H 'Content-Type: application/grpc' -H 'TE: trailers' --data-binary @- \
    -D - -o /dev/null "https://grpc.bee.test:$INGRESS_HTTPS_PORT/bee.v1.AuthService/WhoAmI" | tr -d '\r')"
  grep -qi '^grpc-status:' <<<"$GRPC_HEADERS" && break
  sleep 2
done
say "gRPC response headers: $(tr '\n' ' ' <<<"$GRPC_HEADERS")"
assert "a gRPC call to the gRPC host reaches the server (HTTP/2, grpc-status 16)" grep -qi '^grpc-status: 16' <<<"$GRPC_HEADERS"
for p in ingress ingress-tls; do pf_stop "$p"; done

# ---------------------------------------------------------------------------------------------
phase "helm uninstall"
# ---------------------------------------------------------------------------------------------
for p in server web oidc; do pf_stop "$p"; done
# Not `helm uninstall --wait`: Helm 4.2.1 waits for deletion until --timeout even when everything is
# already gone (the same behaviour as in `helm test`), so the script waits for the pods itself.
hm uninstall "$RELEASE" -n "$NS" >/dev/null
waited=0
until [ "$(kc -n "$NS" get pods -l "app.kubernetes.io/instance=$RELEASE" -o name | wc -l | tr -d ' ')" = 0 ]; do
  [ "$waited" -lt 120 ] || die "the release's pods were still there 120s after helm uninstall"
  sleep 2
  waited=$((waited + 2))
done
LEFT="$(kc -n "$NS" get deploy,svc,cm,pdb,sa,ingress,hpa,pods -l "app.kubernetes.io/instance=$RELEASE" -o name | wc -l | tr -d ' ')"
assert "uninstall left no object of the release behind ($LEFT)" test "$LEFT" = 0
assert "the Secrets the operator created are untouched" test "$(kc -n "$NS" get secret bee-secrets bee-ssh -o name | wc -l | tr -d ' ')" = 2

phase "teardown"
if [ -z "${BEE_HELM_KEEP:-}" ]; then
  "$KIND" delete cluster --name "$CLUSTER" >/dev/null 2>&1
  CLUSTER_CREATED=""
fi
phase ""
print_timings
echo "$NAME: ok"
