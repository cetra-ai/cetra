# Python SDK

`cetra` is the Python SDK for cetra, the single-binary LLM observability
server. It is a thin layer over OpenTelemetry: spans go out as OTLP/HTTP
protobuf to `<base_url>/v1/traces`, and scores go to
`<base_url>/api/v1/scores`. Python >= 3.11. MIT.

The server stores traces only. If another OpenTelemetry GenAI instrumentation sends prompts
and completions as log events, they reach `/v1/logs` and are dropped; the project shows a hint
in the UI. Set that instrumentation to capture message content on spans.

## Install

```bash
pip install cetra-sdk
```

Auto-instrumentation is optional, one extra per library:

| `instrument` name | Extra |
|---|---|
| `openai` | `cetra-sdk[openai]` |
| `anthropic` | `cetra-sdk[anthropic]` |
| `langchain` | `cetra-sdk[langchain]` |
| `llama-index` | `cetra-sdk[llama-index]` |
| `litellm` | `cetra-sdk[litellm]` |
| `google-genai` | `cetra-sdk[google-genai]` |
| `bedrock` | `cetra-sdk[bedrock]` |
| `openai-agents` | `cetra-sdk[openai-agents]` |
| `crewai` | `cetra-sdk[crewai]` |
| `dspy` | `cetra-sdk[dspy]` |
| `groq` | `cetra-sdk[groq]` |
| `mistralai` | `cetra-sdk[mistralai]` |
| `cohere` | `cetra-sdk[cohere]` |
| `ollama` | `cetra-sdk[ollama]` |
| `together` | `cetra-sdk[together]` |
| `instructor` | `cetra-sdk[instructor]` |
| `agno` | `cetra-sdk[agno]` |
| `smolagents` | `cetra-sdk[smolagents]` |
| `haystack` | `cetra-sdk[haystack]` |
| `beeai` | `cetra-sdk[beeai]` |

The `instructor` and `beeai` distributions do not support every Python
version: the `instructor` extra installs on Python 3.13 or earlier, and `beeai`
on Python 3.14 or earlier.

`cetra-sdk[all]` installs every instrumentor. The core package pulls in only
`opentelemetry-api`, `opentelemetry-sdk`, `opentelemetry-exporter-otlp-proto-http`
and `requests`.

## Quick start

<!-- test -->
```python
import cetra

cetra.init(api_key="sk-cetra-test")


@cetra.observe(kind="tool", name="search_docs")
def search_docs(query: str, limit: int = 2) -> list[str]:
    return ["doc-1", "doc-2"]


@cetra.observe(name="answer")
def answer(question: str) -> str:
    docs = search_docs(question)
    return f"It is in {docs[0]}."


answer("why?")

cetra.flush()  # blocks until spans (and scores) are exported
```

`init()` is called once per process; it also registers an `atexit` hook that
calls `shutdown()` (flush, then stop). `flush()` is the manual lever when you
need the data out now — see [Serverless](https://cetra.sh/docs/reference/python/opentelemetry/#serverless).

Without an API key (argument and `CETRA_API_KEY` both unset), `init()` logs
one warning — `cetra: no API key; set CETRA_API_KEY, or CETRA_ENABLED=false
to silence this; tracing is disabled` — and the SDK runs as a no-op: nothing
raises, nothing is sent. Set `CETRA_ENABLED=false` to silence the warning.

## Documentation

The full guide and reference are at https://cetra.sh/docs/reference/python/ (configuration,
tracing, context, scores, datasets and experiments, OpenTelemetry, the API).
