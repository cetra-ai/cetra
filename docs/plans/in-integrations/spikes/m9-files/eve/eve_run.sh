#!/bin/bash
# Runs inside netrun: starts eve dev on loopback, sends one message, waits, stops eve (its own process only).
set -u
export PATH=<NODE_BIN>:$PATH
BASE=$(cat base.url)
export M9_OPENAI_BASE="$BASE/v1" M9_OTLP="$BASE/v1/traces"
PROJ=<SPIKE_DIR>
cd "$PROJ"
node node_modules/eve/bin/eve.js dev --no-ui --port 2000 --logs warn > "$OLDPWD/eve-dev.log" 2>&1 &
EVE=$!
for i in $(seq 1 300); do curl -s -o /dev/null http://127.0.0.1:2000/ && break; sleep 0.2; done
echo "eve up after $i polls"
curl -s -D "$OLDPWD/eve-post.headers" -X POST http://127.0.0.1:2000/eve/v1/session -H 'content-type: application/json' -d '{"message":"Read hello.txt with the read_file tool and say what it says."}' > "$OLDPWD/eve-post.json"
cat "$OLDPWD/eve-post.json"; echo
SID=$(grep -i "x-eve-session-id" "$OLDPWD/eve-post.headers" | tr -d '\r' | awk '{print $2}')
echo "session: $SID"
timeout 60 curl -s -N "http://127.0.0.1:2000/eve/v1/session/$SID/stream" > "$OLDPWD/eve-stream.txt" || true
sleep 6
kill -INT $EVE 2>/dev/null; sleep 4; kill -TERM $EVE 2>/dev/null; wait $EVE 2>/dev/null
echo "stream lines: $(wc -l < "$OLDPWD/eve-stream.txt")"
