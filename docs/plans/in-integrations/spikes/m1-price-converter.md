# Spike: why LiteLLM's provider rows are missing from the snapshot (M1)

Read 2026-11-23: the converter (`crates/otel/tests/prices.rs`, `just prices-update`) and LiteLLM's `model_prices_and_context_window.json` at the snapshot's source commit `4b498d1bba768bd81e4a434a14ea7ae2870dd01f`.

`model_name()` strips a leading `<litellm_provider>/` and then **skips any key that still contains `/`** ("proxy routes such as `openrouter/…`"). The providers that serve open models name them `org/model`, so almost all their rows are dropped:

| LiteLLM provider | Entries | Still containing `/` after the prefix (dropped) |
|---|---|---|
| groq | 10 | 8 (e.g. `groq/openai/gpt-oss-120b`) |
| novita | 135 | 135 (`novita/deepseek/deepseek-v3.2`) |
| deepinfra | 136 | 136 |
| nebius | 67 | 67 (`nebius/deepseek-ai/DeepSeek-R1`) |
| together_ai | 83 | 75 |
| watsonx | 31 | 30 |
| baseten | 36 | 36 |
| hyperbolic | 16 | 16 |
| cloudflare | 40 | 36 (`cloudflare/@cf/…`) |
| fireworks_ai | 357 | most (`accounts/fireworks/models/…`) |
| vercel_ai_gateway | 99 | 99 (`alibaba/qwen-3-14b`) |
| openrouter, huggingface | 0 | — (LiteLLM has no entries at this commit) |

The part after the prefix is the model id that the client sends to that provider (`openai/gpt-oss-120b` on Groq, `deepseek-ai/DeepSeek-R1` on Nebius), so the rows are correct as they are. Fix (D-IN-14): keep a name that contains `/` once the provider prefix is stripped. A key with no `litellm_provider` prefix and a `/` stays skipped. Rows are still keyed by `(model_name, provider)`, so a nested name never prices another provider's span. OpenRouter and Hugging Face stay without own rows: OpenRouter is priced by D-IN-13's publisher fallback; Hugging Face stays unpriced and its page says so. LiteLLM's `typesafe` provider explains the matrix's "TypeSafe Jev" row (a LiteLLM-listed provider; M2's spike checks it).
