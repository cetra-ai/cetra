import json, threading, http.server, subprocess, sys, os, time, signal, socket
HERE=os.path.dirname(os.path.abspath(__file__))
OUT=os.path.join(HERE,os.environ.get("OUTDIR","out")); os.makedirs(OUT,exist_ok=True)
def free():
    s=socket.socket(); s.bind(("127.0.0.1",0)); p=s.getsockname()[1]; s.close(); return p
REQLOG=[]
class Fake(http.server.BaseHTTPRequestHandler):
    def log_message(self,*a): pass
    def do_POST(self):
        n=int(self.headers.get("content-length",0)); body=json.loads(self.rfile.read(n) or b"{}")
        REQLOG.append((self.path,body.get("model"),body.get("stream")))
        m=body.get("model","")
        rm=m if "/" in m else "gpt-4o-mini-2024-07-18"
        if self.path.endswith("/embeddings"):
            out={"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2]}],"model":"text-embedding-3-small","usage":{"prompt_tokens":5,"total_tokens":5}}
        else:
            has_tool_res=any(x.get("role")=="tool" for x in body.get("messages",[]))
            if body.get("tools") and not has_tool_res and not os.environ.get("NO_TOOL"):
                msg={"role":"assistant","content":None,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]}; fr="tool_calls"; u=(82,17)
            else:
                msg={"role":"assistant","content":"It is 18C and sunny in Paris."}; fr="stop"; u=(131,12)
            out={"id":"chatcmpl-1","object":"chat.completion","created":1,"model":rm,"choices":[{"index":0,"message":msg,"finish_reason":fr}],"usage":{"prompt_tokens":u[0],"completion_tokens":u[1],"total_tokens":sum(u)}}
            if body.get("stream"):
                d={"role":"assistant"}; d.update({k:v for k,v in ({"content":msg["content"]} if msg["content"] else {"tool_calls":[dict(index=0,**msg["tool_calls"][0])]}).items()})
                ch=[{"id":"c","object":"chat.completion.chunk","created":1,"model":rm,"choices":[{"index":0,"delta":d,"finish_reason":None}]},
                    {"id":"c","object":"chat.completion.chunk","created":1,"model":rm,"choices":[{"index":0,"delta":{},"finish_reason":fr}]},
                    {"id":"c","object":"chat.completion.chunk","created":1,"model":rm,"choices":[],"usage":out["usage"]}]
                data=("".join("data: "+json.dumps(c)+"\n\n" for c in ch)+"data: [DONE]\n\n").encode()
                self.send_response(200); self.send_header("content-type","text/event-stream"); self.end_headers(); self.wfile.write(data); return
        data=json.dumps(out).encode()
        self.send_response(200); self.send_header("content-type","application/json"); self.send_header("content-length",str(len(data))); self.end_headers(); self.wfile.write(data)
class Recv(http.server.BaseHTTPRequestHandler):
    n=0
    def log_message(self,*a): pass
    def do_POST(self):
        n=int(self.headers.get("content-length",0)); b=self.rfile.read(n)
        Recv.n+=1
        open(f"{OUT}/recv{Recv.n}.bin","wb").write(b)
        json.dump({"path":self.path,"headers":dict(self.headers)},open(f"{OUT}/recv{Recv.n}.meta.json","w"))
        self.send_response(200); self.send_header("content-type","application/x-protobuf"); self.send_header("content-length","0"); self.end_headers()
def serve(h):
    s=http.server.ThreadingHTTPServer(("127.0.0.1",0),h); threading.Thread(target=serve_f,args=(s,),daemon=True).start(); return s.server_address[1]
def serve_f(s): s.serve_forever()
fp=serve(Fake); rp=serve(Recv); pp=free()
env=dict(os.environ, FAKE_BASE=f"http://127.0.0.1:{fp}/v1", OTEL_EXPORTER="otlp_http", OTEL_ENDPOINT=f"http://127.0.0.1:{rp}", OTEL_HEADERS="Authorization=Bearer%20test-key,x-extra=1",
  OTEL_SERVICE_NAME="litellm-proxy", LITELLM_LOCAL_MODEL_COST_MAP="True", PYTHONPATH=os.path.join(HERE,"hook"), EXT_LOG=os.path.join(OUT,"external.log"), LITELLM_LOG="INFO", DISABLE_SCHEMA_UPDATE="true")
env.update({k:v for k,v in (kv.split("=",1) for kv in os.environ.get("EXTRA_ENV","").split(";") if kv)})
open(os.path.join(OUT,"external.log"),"w").close()
cfg=os.environ.get("CFG","config.yaml")
p=subprocess.Popen([os.path.join(HERE,".venv/bin/litellm"),"--config",os.path.join(HERE,cfg),"--port",str(pp),"--host","127.0.0.1"],env=env,stdout=open(os.path.join(OUT,"proxy.log"),"w"),stderr=subprocess.STDOUT)
import urllib.request
def post(path,body):
    r=urllib.request.Request(f"http://127.0.0.1:{pp}{path}",data=json.dumps(body).encode(),headers={"content-type":"application/json","authorization":"Bearer sk-spike-master-key-1234567890abcdef",**({"traceparent":os.environ["TRACEPARENT"]} if os.environ.get("TRACEPARENT") else {})})
    try: return urllib.request.urlopen(r,timeout=60).read().decode()
    except Exception as e: return "ERR "+str(e)+getattr(e,"read",lambda:b"")().decode()[:300]
for _ in range(120):
    try:
        urllib.request.urlopen(f"http://127.0.0.1:{pp}/health/liveliness",timeout=2); break
    except Exception: time.sleep(1)
tools=[{"type":"function","function":{"name":"get_weather","description":"Get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}]
models=os.environ.get("MODELS","gpt-4o-mini").split(",")
for m in models:
    r=post("/v1/chat/completions",{"model":m,"messages":[{"role":"system","content":"You are a weather bot."},{"role":"user","content":"What is the weather in Paris?"}],"tools":tools,"user":"user-1"})
    print(m,"->",r[:160])
if os.environ.get("MORE"):
    r=post("/v1/chat/completions",{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}],"stream":True,"stream_options":{"include_usage":True}}); print("stream ->",r[:120])
    r=post("/v1/embeddings",{"model":"emb","input":"hello"}); print("emb ->",r[:120])
time.sleep(float(os.environ.get("WAIT","8")))
p.send_signal(signal.SIGINT)
try: p.wait(20)
except Exception: p.kill()
time.sleep(1)
print("upstream requests:",REQLOG); print("receiver posts:",Recv.n); print("external:",open(os.path.join(OUT,"external.log")).read().split() or "none")
