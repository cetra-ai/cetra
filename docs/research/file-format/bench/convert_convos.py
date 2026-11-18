"""Converts public LLM conversation datasets to one Parquet layout that
`fmtbench gen <dir> 0 <files> --convo <max> <file>...` reads.

Output columns, one row per conversation:
  system    string, nullable: the system prompt as plain text
  tools     string, nullable: the tool definitions as a JSON array
  model     string, nullable
  messages  list<string>: OTel GenAI messages as JSON, role user, assistant or tool

Usage: python -I convert_convos.py <openswe|toucan|wildchat> <out.parquet> <max_per_file> <in.parquet>...
"""

import ast
import json
import sys

import pyarrow as pa
import pyarrow.parquet as pq

SCHEMA = pa.schema(
    [
        ("system", pa.string()),
        ("tools", pa.string()),
        ("model", pa.string()),
        ("messages", pa.list_(pa.string())),
    ]
)


def text(s):
    return {"type": "text", "content": s}


def msg(role, parts, finish=None):
    m = {"role": role, "parts": parts}
    if finish:
        m["finish_reason"] = finish
    return m


def _reject(c):
    raise ValueError(c)


def _int(s):
    # Beyond f64's range a JSON reader may reject the number: keep its digits as a string.
    return int(s) if len(s) < 300 else s


def args_obj(a):
    """Tool-call arguments as an object when they parse as JSON, else the raw string."""
    if isinstance(a, str):
        try:
            # NaN and Infinity are not JSON: keep such arguments as the raw string.
            return json.loads(a, parse_constant=_reject, parse_int=_int)
        except ValueError:
            return a
    return a


def openswe(row):
    system, out = None, []
    for m in row["messages"]:
        role, content = m["role"], m["content"] or ""
        if role == "system":
            system = content
        elif role == "user":
            out.append(msg("user", [text(content)]))
        elif role == "assistant":
            parts = []
            if m["reasoning_content"]:
                parts.append({"type": "reasoning", "content": m["reasoning_content"]})
            if content:
                parts.append(text(content))
            calls = m["tool_calls"] or []
            for c in calls:
                f = c["function"] or {}
                parts.append({"type": "tool_call", "id": c["id"], "name": f.get("name"), "arguments": args_obj(f.get("arguments"))})
            out.append(msg("assistant", parts, "tool_calls" if calls else "stop"))
        elif role == "tool":
            out.append(msg("tool", [{"type": "tool_call_response", "id": m.get("tool_call_id"), "response": content}]))
    tools = "[" + ",".join(row["tools"] or []) + "]" if row["tools"] else None
    model = ((row.get("metadata") or {}).get("teacher_model") or {}).get("name")
    return system, tools, model, out


def toucan(row):
    """Kimi-K2 transcripts: consecutive assistant messages (text, then a
    `function_call`) are one model reply; `function` messages are tool results."""
    out, n = [], 0
    last_call = None
    for m in json.loads(row["messages"]):
        role, content = m.get("role"), m.get("content") or ""
        if role == "system":
            continue  # the tool declaration in Kimi's chat template; `tools` carries it
        if role == "user":
            out.append(msg("user", [text(content)]))
        elif role == "assistant":
            parts = [text(content)] if content else []
            fc = m.get("function_call")
            if fc:
                if isinstance(fc, str):
                    fc = ast.literal_eval(fc)
                n += 1
                last_call = f"call_{n}"
                parts.append({"type": "tool_call", "id": last_call, "name": fc.get("name"), "arguments": args_obj(fc.get("arguments"))})
            if out and out[-1]["role"] == "assistant" and out[-1]["finish_reason"] == "stop":
                out[-1]["parts"].extend(parts)
            else:
                out.append(msg("assistant", parts, "stop"))
            if fc:
                out[-1]["finish_reason"] = "tool_calls"
        elif role == "function":
            out.append(msg("tool", [{"type": "tool_call_response", "id": last_call, "response": content}]))
    return None, row["available_tools"], "kimi-k2", out


def wildchat(row):
    out = []
    for m in row["conversation"]:
        role = m["role"]
        if role == "user":
            out.append(msg("user", [text(m["content"] or "")]))
        elif role == "assistant":
            out.append(msg("assistant", [text(m["content"] or "")], "stop"))
    return None, None, row["model"], out


KINDS = {
    "openswe": (openswe, ["messages", "tools", "metadata"]),
    "toucan": (toucan, ["messages", "available_tools"]),
    "wildchat": (wildchat, ["conversation", "model"]),
}


def main():
    kind, out_path, max_per_file, inputs = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4:]
    conv, cols = KINDS[kind]
    rows = {k: [] for k in SCHEMA.names}
    for path in inputs:
        f = pq.ParquetFile(path)
        taken = 0
        for batch in f.iter_batches(batch_size=256, columns=cols):
            for row in batch.to_pylist():
                if taken == max_per_file:
                    break
                system, tools, model, messages = conv(row)
                if not any(m["role"] == "assistant" for m in messages):
                    continue
                rows["system"].append(system)
                rows["tools"].append(tools)
                rows["model"].append(model)
                rows["messages"].append([json.dumps(m, ensure_ascii=False, separators=(",", ":"), allow_nan=False) for m in messages])
                taken += 1
            if taken == max_per_file:
                break
        print(f"{path}: {taken} conversations")
    pq.write_table(pa.table(rows, schema=SCHEMA), out_path, compression="zstd")


if __name__ == "__main__":
    main()
