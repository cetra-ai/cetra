import json, threading, http.server, subprocess, os, time, socket, urllib.request, signal
HERE = os.path.dirname(os.path.abspath(__file__))

def free():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p

REQLOG = []   # (method, path, auth header, model) seen by the fake upstream

class Fake(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_POST(self):
        n = int(self.headers.get("content-length", 0)); body = json.loads(self.rfile.read(n) or b"{}")
        REQLOG.append({"path": self.path, "authorization": self.headers.get("authorization"), "model": body.get("model"), "stream": body.get("stream"), "body": body})
        if os.environ.get("FAKE_FAIL"):
            data = json.dumps({"error": {"message": "fake upstream failure", "type": "server_error", "code": "boom"}}).encode()
            self.send_response(500); self.send_header("content-type", "application/json"); self.send_header("content-length", str(len(data))); self.end_headers(); self.wfile.write(data); return
        if self.path.endswith("/messages"):
            out = {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-3-5-sonnet-20241022", "content": [{"type": "text", "text": "It is 18C and sunny in Paris."}], "stop_reason": "end_turn", "stop_sequence": None, "usage": {"input_tokens": 31, "output_tokens": 12}}
        elif self.path.endswith("/responses"):
            tool_out = any(i.get("type") == "function_call_output" for i in body.get("input", []) if isinstance(i, dict))
            if body.get("tools") and not tool_out:
                output = [{"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}", "status": "completed"}]; u = (82, 17)
            else:
                output = [{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "It is 18C and sunny in Paris.", "annotations": []}]}]; u = (131, 12)
            out = {"id": "resp_1", "object": "response", "created_at": 1, "status": "completed", "model": "gpt-4o-mini-2024-07-18", "output": output, "parallel_tool_calls": True, "tool_choice": "auto", "tools": [], "usage": {"input_tokens": u[0], "output_tokens": u[1], "total_tokens": sum(u), "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}}
        elif body.get("tools") and not any(m.get("role") == "tool" for m in body.get("messages", [])):
            out = {"id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4o-mini-2024-07-18",
                   "choices": [{"index": 0, "message": {"role": "assistant", "content": None, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]}, "finish_reason": "tool_calls"}],
                   "usage": {"prompt_tokens": 82, "completion_tokens": 17, "total_tokens": 99}}
        else:
            out = {"id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4o-mini-2024-07-18",
                   "choices": [{"index": 0, "message": {"role": "assistant", "content": "It is 18C and sunny in Paris."}, "finish_reason": "stop"}],
                   "usage": {"prompt_tokens": 31, "completion_tokens": 12, "total_tokens": 43}}
        data = json.dumps(out).encode()
        self.send_response(200); self.send_header("content-type", "application/json"); self.send_header("content-length", str(len(data))); self.end_headers(); self.wfile.write(data)

    def do_GET(self):
        REQLOG.append({"path": "GET " + self.path, "authorization": self.headers.get("authorization"), "model": None, "stream": None, "body": {}})
        data = b'{"object":"list","data":[]}'
        self.send_response(404); self.send_header("content-type", "application/json"); self.send_header("content-length", str(len(data))); self.end_headers(); self.wfile.write(data)

class Recv(http.server.BaseHTTPRequestHandler):
    n = 0; out = "out"
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_POST(self):
        n = int(self.headers.get("content-length", 0)); b = self.rfile.read(n)
        if self.headers.get("content-encoding") == "gzip":
            import gzip; b = gzip.decompress(b)
        Recv.n += 1
        open(f"{Recv.out}/recv{Recv.n}.bin", "wb").write(b)
        json.dump({"path": self.path, "headers": dict(self.headers), "t": time.time()}, open(f"{Recv.out}/recv{Recv.n}.meta.json", "w"))
        self.send_response(200); self.send_header("content-type", "application/x-protobuf"); self.send_header("content-length", "0"); self.end_headers()

def serve(h):
    s = http.server.ThreadingHTTPServer(("127.0.0.1", 0), h); threading.Thread(target=s.serve_forever, daemon=True).start(); return s.server_address[1]

def wait_http(url, timeout=120, ok=(200,)):
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            r = urllib.request.urlopen(url, timeout=2)
            if r.status in ok: return time.time() - t0
        except urllib.error.HTTPError as e:
            if e.code in ok: return time.time() - t0
        except Exception: pass
        time.sleep(0.25)
    raise TimeoutError(url)

def stop(p, sig=signal.SIGINT, wait=20):
    p.send_signal(sig)
    try: p.wait(wait)
    except Exception: p.kill(); p.wait()
