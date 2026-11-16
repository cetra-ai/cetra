#!/usr/bin/env bash
# Lints the chart and validates its rendered manifests against the Kubernetes schemas, with the
# default values and each file in deploy/helm/cetra/ci/, and checks the two template guards
# (D-N-10). Needs helm and kubeconform; kubeconform downloads the schemas.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
chart=deploy/helm/cetra

for values in "" "$chart/ci/s3-values.yaml" "$chart/ci/full-values.yaml" "$chart/ci/metrics-values.yaml"; do
  args=()
  if [[ -n "$values" ]]; then
    args=(-f "$values")
  fi
  echo "helm-check: ${values:-default values}"
  helm lint --strict "$chart" "${args[@]}"
  helm template cetra "$chart" "${args[@]}" | kubeconform -strict -summary
done

# The guards fail the render.
if helm template cetra "$chart" --set s3.enabled=true >/dev/null 2>&1; then
  echo "helm-check: s3.enabled without s3.bucket rendered" >&2
  exit 1
fi
if helm template cetra "$chart" --set ingress.enabled=true >/dev/null 2>&1; then
  echo "helm-check: ingress.enabled without ingress.host rendered" >&2
  exit 1
fi
echo "helm-check: passed"
