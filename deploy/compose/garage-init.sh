#!/bin/sh
# One-shot setup of the single-node Garage of compose.s3.yaml through its admin API (v2):
# a layout with this node, the access key from .env, and the bucket `cetra` with read and
# write for that key. Safe to run again: each step checks what already exists (D-N-9).
set -eu

api="http://garage:3903"
auth="Authorization: Bearer ${GARAGE_ADMIN_TOKEN}"

call() {
  # call <method> <path> [json body]: prints the body; fails on a non-2xx answer.
  if [ $# -eq 3 ]; then
    curl -sS -f -X "$1" -H "$auth" -H 'Content-Type: application/json' --data "$3" "$api$2"
  else
    curl -sS -f -X "$1" -H "$auth" "$api$2"
  fi
}

i=0
until call GET /v2/GetClusterStatus >/dev/null 2>&1; do
  i=$((i + 1))
  if [ "$i" -ge 60 ]; then
    echo "garage-init: the Garage admin API did not answer within 60 s" >&2
    exit 1
  fi
  sleep 1
done

layout_version="$(call GET /v2/GetClusterStatus | tr -d ' \n' | grep -o '"layoutVersion":[0-9]*' | cut -d: -f2)"
if [ -z "$layout_version" ]; then
  echo "garage-init: no layoutVersion in the GetClusterStatus answer" >&2
  exit 1
fi
if [ "$layout_version" = "0" ]; then
  node="$(call GET '/v2/GetNodeInfo?node=self' | tr -d ' \n' | grep -o '"nodeId":"[0-9a-f]*"' | head -n1 | cut -d'"' -f4)"
  call POST /v2/UpdateClusterLayout \
    "{\"roles\":[{\"id\":\"${node}\",\"zone\":\"dc1\",\"capacity\":100000000000,\"tags\":[]}]}" >/dev/null
  call POST /v2/ApplyClusterLayout '{"version":1}' >/dev/null
  echo "garage-init: layout applied for node ${node}"
fi

if ! call GET "/v2/GetKeyInfo?id=${GARAGE_ACCESS_KEY_ID}" >/dev/null 2>&1; then
  call POST /v2/ImportKey \
    "{\"accessKeyId\":\"${GARAGE_ACCESS_KEY_ID}\",\"secretAccessKey\":\"${GARAGE_SECRET_ACCESS_KEY}\",\"name\":\"cetra\"}" >/dev/null
  echo "garage-init: access key imported"
fi

bucket="$(call GET '/v2/GetBucketInfo?globalAlias=cetra' 2>/dev/null | tr -d ' \n' | grep -o '"id":"[0-9a-f]\{64\}"' | head -n1 | cut -d'"' -f4 || true)"
if [ -z "$bucket" ]; then
  bucket="$(call POST /v2/CreateBucket '{"globalAlias":"cetra"}' | tr -d ' \n' | grep -o '"id":"[0-9a-f]\{64\}"' | head -n1 | cut -d'"' -f4)"
  echo "garage-init: bucket cetra created"
fi

call POST /v2/AllowBucketKey \
  "{\"bucketId\":\"${bucket}\",\"accessKeyId\":\"${GARAGE_ACCESS_KEY_ID}\",\"permissions\":{\"read\":true,\"write\":true,\"owner\":false}}" >/dev/null
echo "garage-init: done"
