#!/usr/bin/env bash
# Smoke-tests a running cetra (D-N-13): readiness, the embedded UI, and one span sent over
# OTLP/HTTP and read back through the API.
#
# Usage: smoke.sh <http_url> <otlp_http_url> <secret_key>
#   http_url       the UI and API listener, e.g. http://127.0.0.1:8080
#   otlp_http_url  the OTLP/HTTP listener, e.g. http://127.0.0.1:4318
#   secret_key     an `sk-cetra-…` key of the project (CETRA_INIT_SECRET_KEY)
#
# Needs bash, curl and python3. Waits up to 120 s for /readyz.
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: smoke.sh <http_url> <otlp_http_url> <secret_key>" >&2
  exit 2
fi
base="${1%/}"
otlp="${2%/}"
key="$3"

ready=0
for _ in $(seq 1 120); do
  if curl -sf "$base/readyz" >/dev/null; then
    ready=1
    break
  fi
  sleep 1
done
if [[ "$ready" -ne 1 ]]; then
  echo "smoke: $base/readyz did not answer 200 within 120 s" >&2
  exit 1
fi
echo "smoke: $base/readyz is ready"

# The embedded UI (D-G-18): the built index.html loads hashed bundles from /assets/.
if ! curl -sf -H 'Accept: text/html' "$base/" | grep -q '/assets/'; then
  echo "smoke: $base/ is not the built web UI" >&2
  exit 1
fi
echo "smoke: the web UI is embedded"

trace_id="$(python3 -c 'import secrets; print(secrets.token_hex(16))')"
span_id="$(python3 -c 'import secrets; print(secrets.token_hex(8))')"
start="$(python3 -c 'import time; print(time.time_ns() - 1_000_000_000)')"
end="$(python3 -c 'import time; print(time.time_ns())')"
body="$(cat <<EOF
{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"cetra-smoke"}}]},"scopeSpans":[{"scope":{"name":"cetra-smoke"},"spans":[{"traceId":"$trace_id","spanId":"$span_id","name":"cetra-smoke","kind":1,"startTimeUnixNano":"$start","endTimeUnixNano":"$end"}]}]}]}
EOF
)"

status="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$otlp/v1/traces" \
  -H "Authorization: Bearer $key" \
  -H 'Content-Type: application/json' \
  --data "$body" || true)"
if [[ "$status" != "200" ]]; then
  echo "smoke: POST $otlp/v1/traces answered $status" >&2
  exit 1
fi
echo "smoke: span sent to $otlp/v1/traces"

# Spans are queryable within seconds (WAL + memtable); poll for 30 s.
for _ in $(seq 1 60); do
  if curl -sf -H "Authorization: Bearer $key" \
    "$base/api/v1/projects/current/traces/$trace_id" 2>/dev/null |
    python3 -c 'import json, sys; sys.exit(0 if json.load(sys.stdin)["trace"]["name"] == "cetra-smoke" else 1)' 2>/dev/null; then
    echo "smoke: trace $trace_id read back"
    exit 0
  fi
  sleep 0.5
done
echo "smoke: trace $trace_id was not readable within 30 s" >&2
exit 1
