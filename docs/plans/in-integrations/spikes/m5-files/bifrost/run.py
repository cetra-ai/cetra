import os, sys, json, time, subprocess
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import *
import common
BIN = os.environ["BIN"]; OUT = os.path.abspath(os.environ.get("OUTDIR", "out_bifrost")); os.makedirs(OUT, exist_ok=True); common.Recv.out = OUT
fp = serve(Fake); rp = serve(Recv); gp = free()
app = os.path.join(OUT, "app"); os.makedirs(app, exist_ok=True)
otel = {"service_name": "bifrost", "collector_url": f"http://127.0.0.1:{rp}/v1/traces", "trace_type": "genai_extension", "protocol": "http", "headers": {"Authorization": "env.CETRA_OTLP_AUTH"}}
cfg = {"providers": {"openai": {"keys": [{"name": "k1", "value": "env.UPSTREAM_KEY", "models": json.loads(os.environ.get("KEY_MODELS", "[\"*\"]")), "weight": 1.0}], "network_config": {"base_url": f"http://127.0.0.1:{fp}"}}},
       "plugins": [{"enabled": True, "name": "otel", "config": otel}]}
import shutil
cfg["client"]={"enforce_auth_on_inference":False}
for f in ("pricing.json","model-parameters.json"): shutil.copy(os.path.join(HERE,"..","dl",{"pricing.json":"lp_pricing.json","model-parameters.json":"lp_mp.json"}[f]), os.path.join(app,f))
if not os.environ.get("NO_LOCAL_PRICING"): cfg["framework"]={"pricing":{"pricing_url":"file://"+app+"/pricing.json","pricing_sync_interval":86400,"model_parameters_url":"file://"+app+"/model-parameters.json","mcp_library_sync_interval":0}}
if os.environ.get("PROFILES"): cfg["plugins"][0]["config"] = {"profiles": [dict(otel, enabled=True)]}
if os.environ.get("CONFIG_EXTRA"): cfg["plugins"][0]["config"].update(json.loads(os.environ["CONFIG_EXTRA"]))
json.dump(cfg, open(os.path.join(app, "config.json"), "w"), indent=1)
env = dict(os.environ, CETRA_OTLP_AUTH="Bearer test-key", UPSTREAM_KEY="sk-upstream-fake")
t0 = time.time()
p = subprocess.Popen([BIN, "-app-dir", app, "-port", str(gp), "-host", "127.0.0.1", "-log-style", "pretty"], env=env, stdout=open(f"{OUT}/gw.log", "w"), stderr=subprocess.STDOUT)
try:
    up = wait_http(f"http://127.0.0.1:{gp}/health"); print("ready after %.2fs" % (time.time() - t0))
except Exception as e:
    print("NOT READY", e); stop(p); raise SystemExit(1)
from openai import OpenAI
c = OpenAI(base_url=f"http://127.0.0.1:{gp}/v1", api_key="sk-client-fake")
treq=time.time()
try:
    r = c.chat.completions.create(model="openai/gpt-4o-mini", messages=[{"role": "system", "content": "You are a weather bot."}, {"role": "user", "content": "What is the weather in Paris?"}], user="user-1", extra_headers=({"traceparent": os.environ["TRACEPARENT"]} if os.environ.get("TRACEPARENT") else {}), **json.loads(os.environ.get("REQ_EXTRA", "{}")))
    print("reply:", r.choices[0].message.content, r.usage, "model:", r.model)
except Exception as e: print("REQUEST ERR", e)
for _ in range(int(float(os.environ.get("WAIT", "6"))*10)):
    if common.Recv.n: break
    time.sleep(0.1)
if common.Recv.n: print("first export %.2fs after the request" % (json.load(open(f"{OUT}/recv1.meta.json"))["t"]-treq))
print("posts before stop:", common.Recv.n)
stop(p); time.sleep(1)
print("posts after stop:", common.Recv.n)
print("upstream:", [(x["path"], x["authorization"], x["model"]) for x in REQLOG])
