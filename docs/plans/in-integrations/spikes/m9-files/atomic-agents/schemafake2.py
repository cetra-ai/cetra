"""M9 spike F helper: in-process OpenAI chat fake that answers ONE tool call (arguments built
from the offered tool's JSON schema: strings "spike", numbers 1, booleans true), then text for
every later call. Saves every request body to $M7_OUT/sfreq/req-NNN.json. Loopback only."""
import json
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CALLS = []
_STATE = {"served_tool": False}
OUT = os.environ.get("M7_OUT", ".")


def args_for(schema):
    out = {}
    props = (schema or {}).get("properties", {})
    for key, spec in props.items():
        t = spec.get("type")
        if t == "string":
            out[key] = "spike"
        elif t in ("integer", "number"):
            out[key] = 1
        elif t == "boolean":
            out[key] = True
        elif t == "array":
            out[key] = []
        elif t == "object":
            out[key] = {}
        else:
            out[key] = "spike"
    return out


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        model = body.get("model", "gpt-4o-mini")
        tools = body.get("tools") or []
        CALLS.append({"path": self.path, "tools": [t.get("function", {}).get("name") for t in tools],
                      "model": model, "tool_choice": body.get("tool_choice")})
        os.makedirs(os.path.join(OUT, "sfreq"), exist_ok=True)
        with open(os.path.join(OUT, "sfreq", f"req-{len(CALLS):03d}.json"), "w") as h:
            json.dump(body, h, indent=1)
        usage = {"prompt_tokens": 82, "completion_tokens": 17, "total_tokens": 99}
        if tools and not _STATE["served_tool"]:
            _STATE["served_tool"] = True
            fn = tools[0]["function"]
            msg = {"role": "assistant", "content": None, "tool_calls": [
                {"id": "call_m9_1", "type": "function",
                 "function": {"name": fn["name"], "arguments": json.dumps(args_for(fn.get("parameters")))}}]}
            finish = "tool_calls"
        else:
            msg = {"role": "assistant", "content": "It is 18C and sunny in Paris."}
            finish = "stop"
            usage = {"prompt_tokens": 131, "completion_tokens": 12, "total_tokens": 143}
        resp = {"id": "chatcmpl-m9", "object": "chat.completion", "created": 1, "model": model,
                "choices": [{"index": 0, "message": msg, "finish_reason": finish}], "usage": usage}
        data = json.dumps(resp).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def start(port=18081):
    srv = ThreadingHTTPServer(("127.0.0.1", port), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv
