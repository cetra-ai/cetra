#!/usr/bin/env bash
# Creates a kind cluster, loads an image, installs the chart with it and runs smoke.sh through
# `kubectl port-forward`, then deletes the cluster (D-N-10, D-N-13).
#
# Usage: helm-smoke.sh <image>, e.g. helm-smoke.sh cetra:dev. Needs docker, kind, kubectl, helm,
# python3. The release is named cetra, so the chart's fullname, the Service and the Secret are
# all `cetra` and the pod is `cetra-0`.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
if [[ $# -ne 1 ]]; then
  echo "usage: helm-smoke.sh <image>" >&2
  exit 2
fi
image="$1"
repository="${image%:*}"
tag="${image##*:}"
cluster="cetra-smoke-$$"
context="kind-$cluster"
key="sk-cetra-SMOKE0123456789ABCDEFGHIJKLMNOPQ"
forward_pid=""
log="$(mktemp)"

cleanup() {
  if [[ -n "$forward_pid" ]]; then
    kill "$forward_pid" 2>/dev/null || true
  fi
  kind delete cluster --name "$cluster" >/dev/null 2>&1 || true
  rm -f "$log"
}
trap cleanup EXIT

kind create cluster --name "$cluster" --wait 180s
kind load docker-image "$image" --name "$cluster"

if ! helm install cetra deploy/helm/cetra --kube-context "$context" \
  --set image.repository="$repository" \
  --set image.tag="$tag" \
  --set image.pullPolicy=Never \
  --set persistence.size=1Gi \
  --set-string secret.values.CETRA_INIT_ADMIN_EMAIL=smoke@example.com \
  --set-string secret.values.CETRA_INIT_ADMIN_PASSWORD=cetra-smoke-1 \
  --set-string secret.values.CETRA_INIT_ORG=Smoke \
  --set-string secret.values.CETRA_INIT_PROJECT=Smoke \
  --set-string secret.values.CETRA_INIT_SECRET_KEY="$key" \
  --wait --timeout 11m; then
  kubectl --context "$context" describe pod cetra-0 >&2 || true
  kubectl --context "$context" logs cetra-0 >&2 || true
  exit 1
fi

read -r http_port otlp_port < <(python3 -c '
import socket

socks = [socket.socket() for _ in range(2)]
for sock in socks:
    sock.bind(("127.0.0.1", 0))
print(*(sock.getsockname()[1] for sock in socks))
for sock in socks:
    sock.close()
')
if [[ -z "${http_port:-}" || -z "${otlp_port:-}" ]]; then
  echo "helm-smoke: could not pick free ports (is python3 installed?)" >&2
  exit 1
fi
kubectl --context "$context" port-forward svc/cetra "$http_port:8080" "$otlp_port:4318" >"$log" 2>&1 &
forward_pid=$!

if ! tools/packaging/smoke.sh "http://127.0.0.1:$http_port" "http://127.0.0.1:$otlp_port" "$key"; then
  cat "$log" >&2
  kubectl --context "$context" logs cetra-0 >&2 || true
  exit 1
fi
echo "helm-smoke: $image passed on kind"
