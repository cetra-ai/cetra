import os, sys, json, time, subprocess
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import *
class SystemExit_(Exception): pass
import common
BIN = os.environ["BIN"]; OUT = os.path.abspath(os.environ.get("OUTDIR", "out_aigw")); os.makedirs(OUT, exist_ok=True); common.Recv.out = OUT
fp = serve(Fake); rp = serve(Recv); lp = free(); ap = free()
tmpl = open(os.path.join(HERE, "..", os.environ.get("TMPL", "aigw.tmpl.yaml"))).read().replace("@LISTEN@", str(lp)).replace("@FAKE@", str(fp))
cfgp = os.path.join(OUT, "aigw.yaml"); open(cfgp, "w").write(tmpl)
env = {k: v for k, v in os.environ.items()}
env.update(OPENAI_API_KEY="sk-upstream-fake", OTEL_SERVICE_NAME="aigw", OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=f"http://127.0.0.1:{rp}/v1/traces",
  OTEL_EXPORTER_OTLP_PROTOCOL="http/protobuf", OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer test-key", OTEL_METRICS_EXPORTER="none", OTEL_LOGS_EXPORTER="none",
  OTEL_TRACES_EXPORTER="otlp", ENVOY_VERSION="1.39.1")
env.update({k: v for k, v in (kv.split("=", 1) for kv in os.environ.get("EXTRA_ENV", "").split(";") if kv)})
t0 = time.time()
p = subprocess.Popen([BIN, "run", cfgp, "--admin-port", str(ap)] + (["--debug"] if os.environ.get("DEBUG") else []), env=env, stdout=open(f"{OUT}/gw.log", "w"), stderr=subprocess.STDOUT)
try:
    wait_http(f"http://127.0.0.1:{ap}/health", ok=(200,)); t1 = time.time() - t0
    # the admin port answers before Envoy listens; poll the data port
    t2 = wait_http(f"http://127.0.0.1:{lp}/v1/models", ok=(200, 404, 503)); print("admin ready %.2fs, listener answers %.2fs" % (t1, time.time() - t0))
except Exception as e:
    print("NOT READY", e); stop(p); raise SystemExit(1)
from openai import OpenAI
c = OpenAI(base_url=f"http://127.0.0.1:{lp}/v1", api_key="sk-client-fake", max_retries=int(os.environ.get("RETRIES", "0")))
treq = time.time()
if os.environ.get("RAW_PATH"):
    rq = urllib.request.Request(f"http://127.0.0.1:{lp}{os.environ['RAW_PATH']}", data=os.environ["RAW_BODY"].encode(), headers={"content-type": "application/json", "x-api-key": "sk-client", "anthropic-version": "2023-06-01"})
    try: print("raw reply:", urllib.request.urlopen(rq, timeout=20).read().decode()[:200])
    except Exception as e: print("RAW ERR", e, getattr(e, "read", lambda: b"")()[:200])
try:
    if os.environ.get("RAW_PATH"): raise SystemExit_()
    r = c.chat.completions.create(model=os.environ.get("MODEL", "gpt-4o-mini"), messages=[{"role": "system", "content": "You are a weather bot."}, {"role": "user", "content": "What is the weather in Paris?"}], user="user-1", extra_headers=({"traceparent": os.environ["TRACEPARENT"]} if os.environ.get("TRACEPARENT") else {}), **json.loads(os.environ.get("REQ_EXTRA", "{}")))
    print("reply:", r.choices[0].message.content, r.usage, "model:", r.model)
except Exception as e: print("REQUEST ERR", e)
for _ in range(int(float(os.environ.get("WAIT", "6")) * 10)):
    if common.Recv.n: break
    time.sleep(0.1)
if common.Recv.n: print("first export %.2fs after the request" % (json.load(open(f"{OUT}/recv1.meta.json"))["t"] - treq))
time.sleep(1)
print("posts before stop:", common.Recv.n)
t_stop=time.time(); stop(p); print("aigw exited in %.2fs with code %s" % (time.time()-t_stop, p.returncode)); time.sleep(1)
print("envoy left:", subprocess.run("pgrep -f envoy-versions", shell=True, capture_output=True, text=True).stdout.split())
print("listening ports during run (ss) were not recorded")
print("posts after stop:", common.Recv.n)
print("upstream:", [(x["path"], x["authorization"], x["model"]) for x in REQLOG])
