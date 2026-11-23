# Track IN, M1: provider attribution — plan

Calls through the OpenAI client to providers other than OpenAI get the right provider and cost. The normalizer derives the provider from the endpoint's host (D-IN-2), learns the new aliases and the framework-name table (D-IN-15), and prices gateway spans by the model's publisher (D-IN-13). The price converter keeps LiteLLM's `vendor/model` rows, and the snapshot is refreshed (D-IN-14). The capture harness reaches the mock under real hostnames (D-IN-3), and the mock matches routes by path suffix (D-IN-4). cetra's Python SDK stamps the OpenAI client's host on spans (D-IN-18). Four captured cases pin the result.

**Prerequisites.** The branch `claude/in-integrations` at or after the commit that adds this plan. Toolchain as in `AGENTS.md` § Commands: `uv`, `pnpm`, `cargo-nextest`, and Node on `PATH`:

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

**Read first.**
- `docs/plans/in-integrations/00-design.md`: § Provider from the endpoint, and decisions D-IN-2, D-IN-3, D-IN-4, D-IN-13, D-IN-14, D-IN-15, D-IN-18.
- `docs/plans/in-integrations/spikes/m1-host-keys.md`, `m1-price-converter.md`, `m1-ts-host-stamp.md`.
- `docs/spec/02-ingestion.md`: § Model, provider, usage (the `provider` row, line ~131), rule 12 (line ~205), § Cost computation step 2 (line ~349) and the built-in prices paragraph (line ~355), § Testing (line ~367).
- `docs/spec/10-sdk.md` § Instrumentors (line ~237).
- Code:
  - `crates/otel/src/context.rs` (`PROVIDER`, `ADK_FRAMEWORK`, `provider`, `canonical_provider`, `exact_alias`, `WELL_KNOWN_PROVIDERS`, tests);
  - `crates/otel/src/normalize.rs` `accept_span` (lines ~207–295);
  - `crates/otel/src/cost.rs` (`apply`, `lookup`, `ranked_lookup`, `row_matches`, `same_provider`, tests);
  - `crates/otel/tests/prices.rs`, and `crates/otel/tests/fixtures.rs` (`pick_price`, `price_row_matches`, `same_provider`, `check_computed`);
  - `crates/otel/tests/golden.rs` (`captured_llm_spans_are_priced_by_the_builtin_snapshot`);
  - `tools/capture/src/{scenario,mock}.rs`, `tools/capture/py/src/capture_py/harness.py`, `tools/capture/py/scenarios/openinference/openai-chat-tool/`;
  - `sdks/python/src/cetra/{_instrument,_processor,_runtime}.py` and `sdks/python/tests/{conftest,test_instrument}.py`.
- `fixtures/otlp/README.md` § Workflows.

## What it is, and what it is not

When it is done:
- An `llm` or `embedding` span whose provider is empty or `openai`, and which carries a known host in `server.address`, `url.full` or `gen_ai.openai.api_base`, gets that host's provider.
- `fireworks`, `together`, `watsonx` and LiteLLM's ids resolve to their semconv names, and `autogen` or `spring_ai` in `gen_ai.system` is skipped like `gcp.vertex.agent`.
- An OpenRouter, Vercel AI Gateway, Helicone, Portkey or Cloudflare AI Gateway span on `openai/gpt-4o-mini` is priced by OpenAI's row.
- The snapshot has Groq, Novita, Nebius, DeepInfra, Baseten, Hyperbolic, watsonx, Together and Vercel AI Gateway rows.
- `cetra.init(instrument=["openai"])` puts `server.address` on OpenAI-client spans.
- The capture mock serves a scenario turn under a real host such as `openrouter.ai`.

Not in M1:
- Provider pages, the index regrouping, `check-integrations.mjs`, the Cohere and Ollama mock routes, and new registry rows (M2).
- Gateway pages and gateway-reported cost (M5).
- OpenAI v2 captured cases. OpenAI v2 writes no host, and cetra's stamp is tied to the OpenInference `openai` instrumentor (D-IN-18), so an OpenAI v2 span on an unknown host stays `openai`. A captured case would pin only that, and would fail `captured_llm_spans_are_priced_by_the_builtin_snapshot`. The design's "six cases" becomes four (D-IN-19).
- TypeScript changes: OpenInference JS already writes `server.address` (`spikes/m1-ts-host-stamp.md`).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IN-2 | Provider from the endpoint: `llm`/`embedding`, provider empty or `openai`, host from `server.address` → `url.full` → `gen_ai.openai.api_base`, fixed table, attributes kept. |
| D-IN-3 | Real hosts reach the mock through an injected HTTP client; the instrumentation sees the real host. |
| D-IN-4 | Mock routes match by path suffix. |
| D-IN-13 | Gateway spans fall back to the publisher named by `vendor/name`, through a fixed vendor table. |
| D-IN-14 | The converter keeps names with `/` after the provider prefix; every snapshot provider is a fixed point of rule 12. |
| D-IN-15 | New aliases; framework names in `gen_ai.system` are skipped. |
| D-IN-18 | The Python SDK stamps `server.address` on OpenAI-client spans when `instrument` names `openai`. |
| D-IN-19 (new, this plan) | M1's captured cases are four: OpenRouter and Novita through cetra's SDK with OpenInference, and through OpenLLMetry. The OpenInference cases run through cetra's runtime (the test seam `Runtime.create(config, exporter=…)`), so they pin the stamp. LiteLLM's `clf_ai_gateway` is renamed `cloudflare_ai_gateway` by the converter and by rule 12. The mock checks a turn's `host` against the request's `Host` header. |

User answers that apply: D-IN-18's SDK host stamp (the user, 2026-11-23).

## Versions

No new third-party dependency.
- `tools/capture/py` gains direct dependencies on `httpx`, at exactly the version already in `tools/capture/py/uv.lock`, and on `cetra-sdk` as a path dependency on `../../../sdks/python`.
- The price snapshot source stays LiteLLM commit `4b498d1bba768bd81e4a434a14ea7ae2870dd01f`, the current snapshot's source, so the refresh diff shows only the converter fix.

The implementer reports the `httpx` version used.

## Phases and dependencies

| Phase | Edits | Depends on |
|---|---|---|
| IN1.1 Host rule, aliases, framework names | `crates/otel/src/context.rs`, `crates/otel/src/normalize.rs`, `docs/spec/02-ingestion.md` (§ Model, provider, usage, rule 12) | — |
| IN1.2 Gateway pricing | `crates/otel/src/cost.rs`, `crates/otel/src/lib.rs`, `crates/otel/tests/fixtures.rs`, `docs/spec/02-ingestion.md` (§ Cost computation step 2) | IN1.1 |
| IN1.3 Price converter and snapshot | `crates/otel/tests/prices.rs`, `crates/otel/prices/builtin.json`, `crates/otel/src/context.rs` (one test), `docs/spec/02-ingestion.md` (built-in prices paragraph) | IN1.2 |
| IN1.4 Capture harness | `tools/capture/src/{scenario,mock,write}.rs`, `tools/capture/src/wire/{openai,anthropic,gemini}.rs`, `tools/capture/py/{pyproject.toml,uv.lock}`, `tools/capture/py/src/capture_py/harness.py` | — |
| IN1.5 SDK host stamp | `sdks/python/src/cetra/{_host.py (new),_instrument.py,_processor.py}`, `sdks/python/tests/test_instrument.py`, `docs/spec/10-sdk.md`, `website/src/content/docs/docs/reference/python/opentelemetry.mdx` | — |
| IN1.6 Captured cases | `tools/capture/py/scenarios/{openinference,openllmetry}/{openrouter-chat,novita-chat}/`, `fixtures/otlp/{openinference,openllmetry}/{openrouter-chat,novita-chat}/`, `fixtures/otlp/prices.json`, `docs/spec/02-ingestion.md` (§ Testing), `CHANGELOG.md`, `docs/roadmap.md`, `docs/mvp-decisions.md` (D-IN-19) | IN1.1–IN1.5 |

IN1.1 → IN1.2 → IN1.3 run in order, because they share `docs/spec/02-ingestion.md` and `crates/otel`. IN1.4 and IN1.5 can run alongside them, in their own worktrees cut from the same base. IN1.6 runs last.

**Rules for every phase.** Run only the tests named in "Done when", not the full suites (the machine is shared). Compile Rust once, right before the commit. Change no file outside the phase's Edit list. Commit once, with the given subject. If a step is unclear or contradicts the code, stop and report instead of improvising.

---

## IN1.1 — Host rule, aliases, framework names

**Goal:** the provider comes from the endpoint's host when the instrumentation left it empty or `openai`, and rule 12 knows the new names.

**Edit:** `crates/otel/src/context.rs`, `crates/otel/src/normalize.rs`, `docs/spec/02-ingestion.md`.

**Steps.**

1. In `context.rs`, replace `const ADK_FRAMEWORK: &str = "gcp.vertex.agent";` with:

   ```rust
   /// Values of `gen_ai.system` that name a framework, never a provider (D-B1-8,
   /// D-IN-15). Compared after lowercasing. Sorted (test `framework_names_are_sorted`).
   const FRAMEWORK_NAMES: &[&str] = &["autogen", "gcp.vertex.agent", "spring_ai"];
   ```

   In `provider()`, change the filter so that a `gen_ai.system` value is skipped when `FRAMEWORK_NAMES.binary_search(&value.to_lowercase().as_str()).is_ok()`. Every other key is never skipped. A skipped key stays unread (it is never `take`n), as today. Update the doc comment to cite D-IN-15.

2. Extend `exact_alias` with these arms (keep the existing ones):

   ```rust
   "fireworks" => "fireworks_ai",
   "together" | "togetherai" => "together_ai",
   "watsonx" | "ibm" => "ibm.watsonx.ai",
   "oracle" => "oci",
   "bedrock" => "aws.bedrock",
   "azure_ai" => "azure.ai.inference",
   "mistral" => "mistral_ai",
   "ollama_chat" => "ollama",
   "cohere_chat" => "cohere",
   "text-completion-openai" => "openai",
   "clf_ai_gateway" => "cloudflare_ai_gateway",
   ```

   In `canonical_provider`, before the `exact_alias` call, add: `if lower.starts_with("vertex_ai-") { return "gcp.vertex_ai".to_owned(); }`.

3. Add the host table and the host rule to `context.rs`:

   ```rust
   /// Keys that carry the endpoint, in order (D-IN-2). Read with `get`, never consumed.
   const HOST_KEYS: &[&str] = &["server.address", "url.full", "gen_ai.openai.api_base"];

   /// Endpoint host → provider (D-IN-2). An entry starting with `*.` matches a host
   /// that ends with the rest preceded by a dot. The first match wins.
   const ENDPOINT_PROVIDERS: &[(&str, &str)] = &[
       ("openrouter.ai", "openrouter"),
       ("api.groq.com", "groq"),
       ("api.x.ai", "x_ai"),
       ("api.deepseek.com", "deepseek"),
       ("api.fireworks.ai", "fireworks_ai"),
       ("api.together.xyz", "together_ai"),
       ("api.together.ai", "together_ai"),
       ("api.novita.ai", "novita"),
       ("dashscope.aliyuncs.com", "dashscope"),
       ("dashscope-intl.aliyuncs.com", "dashscope"),
       ("router.huggingface.co", "huggingface"),
       ("api.cloudflare.com", "cloudflare"),
       ("*.cloud.databricks.com", "databricks"),
       ("*.azuredatabricks.net", "databricks"),
       ("api.cerebras.ai", "cerebras"),
       ("api.perplexity.ai", "perplexity"),
       ("api.moonshot.ai", "moonshot"),
       ("api.moonshot.cn", "moonshot"),
       ("api.z.ai", "zai"),
       ("open.bigmodel.cn", "zai"),
       ("api.sambanova.ai", "sambanova"),
       ("api.studio.nebius.com", "nebius"),
       ("api.tokenfactory.nebius.com", "nebius"),
       ("api.mistral.ai", "mistral_ai"),
       ("*.openai.azure.com", "azure.ai.openai"),
       ("api.deepinfra.com", "deepinfra"),
       ("inference.baseten.co", "baseten"),
       ("api.hyperbolic.xyz", "hyperbolic"),
       ("integrate.api.nvidia.com", "nvidia_nim"),
       ("api.minimax.io", "minimax"),
       ("ai-gateway.vercel.sh", "vercel_ai_gateway"),
       ("oai.helicone.ai", "helicone"),
       ("ai-gateway.helicone.ai", "helicone"),
       ("api.portkey.ai", "portkey"),
       ("gateway.ai.cloudflare.com", "cloudflare_ai_gateway"),
   ];

   /// The provider after the endpoint rule (D-IN-2, spec 02 § Provider from the endpoint).
   /// `provider` is the value `fill` resolved. Only `llm` and `embedding` spans whose
   /// provider is empty or `openai` change; the host keys stay in `attributes`.
   pub(crate) fn endpoint_provider(
       bag: &AttrBag<'_>,
       kind: SpanKind,
       provider: Option<String>,
   ) -> Option<String>
   ```

   The body, in order:
   - Return `provider` unchanged unless `kind` is `SpanKind::Llm` or `SpanKind::Embedding`.
   - Return `provider` unchanged if it is `Some(p)` with `p != "openai"`.
   - For each key of `HOST_KEYS`, in order: read `bag.get(key).and_then(as_str)` (`crate::value::as_str`). If that gives a string, compute `host_of(value)`. The first key that yields `Some(host)` decides. Later keys are not read.
   - If a host was found and `match_endpoint(&host)` gives `Some(name)`, return `Some(name.to_owned())`. Otherwise return `provider`.
   - It never panics, never logs, and never calls `take`.

4. Add the two helpers to `context.rs`, with these exact bodies:

   ```rust
   /// The lowercased host of a bare host or a URL: no scheme, user-info, port, path,
   /// query, fragment or trailing dot. `None` when nothing is left.
   fn host_of(value: &str) -> Option<String> {
       let rest = value.trim();
       let rest = rest.split_once("://").map_or(rest, |(_, after)| after);
       let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
       let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
       let host = if let Some(bracketed) = authority.strip_prefix('[') {
           bracketed.split_once(']').map_or(bracketed, |(inside, _)| inside)
       } else {
           authority.split_once(':').map_or(authority, |(name, _)| name)
       };
       let host = host.trim_end_matches('.').to_ascii_lowercase();
       (!host.is_empty()).then_some(host)
   }

   fn match_endpoint(host: &str) -> Option<&'static str> {
       ENDPOINT_PROVIDERS.iter().find_map(|(pattern, provider)| {
           let matched = match pattern.strip_prefix("*.") {
               Some(suffix) => host
                   .strip_suffix(suffix)
                   .is_some_and(|head| head.len() > 1 && head.ends_with('.')),
               None => host == *pattern,
           };
           matched.then_some(*provider)
       })
   }
   ```

   (`split(…).next()` always returns `Some`; the `unwrap_or("")` is only there to avoid `unwrap`.)

5. In `normalize.rs` `accept_span`, right after `agents::context(agent, &span.attributes, &mut bag, &mut fields, batch.agents);` and before the `usage` match, add:

   ```rust
   fields.provider = crate::context::endpoint_provider(&bag, span_kind, fields.provider.take());
   ```

   Import what is needed. Nothing else in `normalize.rs` changes.

6. Spec 02:
   - In § Model, provider, usage, the `provider` row: after `ai.model.provider` (rule 12), add "then the endpoint (§ Provider from the endpoint)". Replace the sentence about `gen_ai.system = gcp.vertex.agent` with: "A `gen_ai.system` value in rule 12's framework-name table is skipped and stays unread (D-B1-8, D-IN-15)."
   - Add a subsection `### Provider from the endpoint` right after that table, before `### Token semantics`. It states the rule of step 3 in prose (D-IN-2) and gives the host table of step 3 as a two-column table (Host, Provider).
   - In rule 12's "Exact:" bullet, add the aliases of step 2 and the `vertex_ai-*` prefix.
   - Add a bullet to rule 12: "Framework names, never providers: `autogen`, `gcp.vertex.agent`, `spring_ai`. A `gen_ai.system` value in this list (compared lowercased) is skipped (D-IN-15)."

**Tests** (in `context.rs` `mod tests`, using the existing `filled` and `string_attr` helpers; add a helper `endpoint(attrs: &[KeyValue], kind: SpanKind, provider: Option<&str>) -> Option<String>` that builds an `AttrBag` the way `filled` does and calls `endpoint_provider`):
- `provider_uses_precedence_and_the_alias_table`: add rows for every new alias of step 2 (one row per left-hand value), and `vertex_ai-anthropic_models` → `gcp.vertex_ai`.
- `framework_names_are_sorted`: `FRAMEWORK_NAMES` is sorted and deduplicated.
- `framework_names_in_gen_ai_system_are_skipped`: `gen_ai.system=autogen` alone gives no provider, and the attribute stays unread (`bag.state("gen_ai.system") == Some(Use::Unread)`). `gen_ai.system=AutoGen` is skipped too. `gen_ai.system=spring_ai` with `llm.provider=openai` gives `openai`.
- `host_of_reads_hosts_and_urls`: `api.groq.com` → `api.groq.com`; `https://OpenRouter.ai/api/v1/` → `openrouter.ai`; `http://user:pw@api.novita.ai:8443/openai?x=1#f` → `api.novita.ai`; `https://x.openai.azure.com./openai` → `x.openai.azure.com`; `http://[::1]:11434/v1` → `::1`; `""` → `None`; `"https://"` → `None`.
- `match_endpoint_uses_exact_hosts_and_label_suffixes`: `openrouter.ai` → `openrouter`; `api.openrouter.ai` → `None`; `res.openai.azure.com` → `azure.ai.openai`; `openai.azure.com` → `None`; `evilopenai.azure.com` → `None`; `a.b.cloud.databricks.com` → `databricks`; `api.openai.com` → `None`.
- `endpoint_rule_applies_to_llm_and_embedding_spans_only`: `server.address=api.groq.com` with provider `None` gives `groq` for `Llm` and `Embedding`, and `None` for `Tool` and `Span`.
- `endpoint_rule_replaces_only_empty_or_openai`: provider `openai` → `groq`; provider `anthropic` stays `anthropic`; provider `None` → `groq`.
- `endpoint_rule_reads_keys_in_order`: `server.address=api.groq.com` plus `gen_ai.openai.api_base=https://api.novita.ai/openai/` → `groq`. Only `url.full=https://api.novita.ai/openai/chat/completions` → `novita`. Only `gen_ai.openai.api_base=https://openrouter.ai/api/v1/` → `openrouter`. `server.address=localhost` with provider `openai` → `openai`. `server.address=""` and `url.full=https://api.x.ai/v1` → `x_ai` (an empty string yields no host, so the next key is read).
- `endpoint_keys_are_not_consumed`: after the rule, `bag.state(key)` is `Some(Use::Unread)` for the key that matched.

**Done when:**

```bash
cargo nextest run -p cetra-otel --lib context::tests
cargo nextest run -p cetra-otel --test golden --test fixtures
```

Both pass. No `expected.json` changes: no existing captured span carries a host in the table.

**Commit:** `Derive the provider from the endpoint host and add rule 12 aliases`

---

## IN1.2 — Gateway pricing

**Goal:** a gateway span that no row of the gateway prices is priced by the publisher named in its `vendor/name` model.

**Edit:** `crates/otel/src/cost.rs`, `crates/otel/src/lib.rs`, `crates/otel/tests/fixtures.rs`, `docs/spec/02-ingestion.md`.

**Steps.**

1. In `cost.rs`, add:

   ```rust
   /// Gateways whose spans fall back to the model publisher's price (D-IN-13). Sorted.
   const GATEWAY_PROVIDERS: &[&str] = &[
       "cloudflare_ai_gateway",
       "helicone",
       "openrouter",
       "portkey",
       "vercel_ai_gateway",
   ];

   /// The vendor segment of a gateway model name → the publisher's provider (D-IN-13). Sorted by vendor.
   const PUBLISHER_VENDORS: &[(&str, &str)] = &[
       ("anthropic", "anthropic"),
       ("cohere", "cohere"),
       ("deepseek", "deepseek"),
       ("google", "gcp.gemini"),
       ("mistralai", "mistral_ai"),
       ("openai", "openai"),
       ("x-ai", "x_ai"),
   ];

   /// For a gateway span, the publisher provider and the model names to try, in order;
   /// `None` when the span is not a gateway span or its model names no known publisher
   /// (D-IN-13, spec 02 § Cost computation step 2).
   pub fn publisher_candidates(provider: &str, model: &str) -> Option<(&'static str, Vec<String>)>
   ```

   The body, in order:
   - Return `None` unless `GATEWAY_PROVIDERS.binary_search(&provider).is_ok()`.
   - Split `model` at the first `/` into `(vendor, name)`. With no `/`, return `None`.
   - Look up `vendor.to_ascii_lowercase()` in `PUBLISHER_VENDORS`. Not found: `None`.
   - If `name` ends with `:free`, return `None`.
   - Cut `name` at its first `:` (keep what is before it). If the result is empty, return `None`.
   - The candidates are `[name]`. If `name` contains `.`, also push `name.replace('.', "-")`.
   - Return `Some((publisher, candidates))`.

2. In `apply`, replace the `let Some(matched) = lookup(prices, model, charge.provider, charge.start_time) else { return none(); };` statement with a first lookup as today, then, when it returns `None` and `charge.provider` is `Some(p)` with `publisher_candidates(p, model)` giving `Some((publisher, names))`: for each name in order, `lookup(prices, name, Some(publisher), charge.start_time)`. The first `Some` is the match. No match at all returns `none()`. `inferred` stays `None` for these spans, because `charge.provider` is `Some`. `cost_source` stays `Computed`.

3. In `lib.rs`, add `publisher_candidates` to the existing `pub use cost::{…}` list.

4. In `crates/otel/tests/fixtures.rs`, where `check_computed` (or its caller) calls `pick_price(rows, model, provider, start_time)`: when that returns `None` and `provider` is `Some(p)`, call `cetra_otel::publisher_candidates(p, model)` and try `pick_price(rows, name, Some(publisher), start_time)` for each name in order. Update the comment above `same_provider` to say the file mirrors `cost.rs`, including the gateway fallback (D-IN-13).

5. Spec 02 § Cost computation step 2: after the sentence "There is no other fallback: a row never prices a span of another provider, …", add: "Gateways are the exception (D-IN-13). When no row matches a span whose provider is `cloudflare_ai_gateway`, `helicone`, `openrouter`, `portkey` or `vercel_ai_gateway`, and its model is `vendor/name` with `vendor` in this table (`anthropic` → `anthropic`, `cohere` → `cohere`, `deepseek` → `deepseek`, `google` → `gcp.gemini`, `mistralai` → `mistral_ai`, `openai` → `openai`, `x-ai` → `x_ai`), the lookup is repeated for that publisher's provider with `name` cut at its first `:`, then with `.` replaced by `-`. A model ending in `:free` is not priced this way. The span keeps its gateway provider."

**Tests** (in `cost.rs` `mod tests`, using `row`, `table` and `priced_of`):
- `publisher_candidates_follow_the_vendor_table`:
  - (`openrouter`, `openai/gpt-4o-mini`) → (`openai`, [`gpt-4o-mini`]);
  - (`openrouter`, `anthropic/claude-sonnet-4.5`) → (`anthropic`, [`claude-sonnet-4.5`, `claude-sonnet-4-5`]);
  - (`openrouter`, `x-ai/grok-4:nitro`) → (`x_ai`, [`grok-4`]);
  - (`openrouter`, `meta-llama/llama-3.3-70b`) → `None`;
  - (`openrouter`, `openai/gpt-oss-20b:free`) → `None`;
  - (`openrouter`, `gpt-4o`) → `None`;
  - (`groq`, `openai/gpt-oss-120b`) → `None`;
  - (`vercel_ai_gateway`, `OpenAI/gpt-4o`) → (`openai`, [`gpt-4o`]).
- `a_gateway_span_is_priced_by_the_publisher_row`: a table with an `openai` row for `gpt-4o-mini`; a span with provider `openrouter` and model `openai/gpt-4o-mini` gets `cost_source` `Computed` and the OpenAI price, and keeps provider `openrouter` (`priced.provider` is `None`).
- `a_gateway_row_beats_the_publisher_fallback`: with an `openrouter` row for `openai/gpt-4o-mini` at another price, that row is used.
- `the_fallback_never_uses_another_reseller`: a table with only a `groq` row for `gpt-4o-mini`; the `openrouter` span on `openai/gpt-4o-mini` is unpriced (`cost_source` `None`).
- `a_non_gateway_span_gets_no_fallback`: provider `novita`, model `openai/gpt-4o-mini`, with an `openai` row only: unpriced.
- `dots_become_dashes_in_the_second_candidate`: an `anthropic` row `claude-sonnet-4-5` prices `openrouter` + `anthropic/claude-sonnet-4.5`.

**Done when:**

```bash
cargo nextest run -p cetra-otel --lib cost::tests
cargo nextest run -p cetra-otel --test fixtures --test golden
```

All pass.

**Commit:** `Price gateway spans by the model publisher`

---

## IN1.3 — Price converter and snapshot

**Goal:** the converter keeps LiteLLM's `vendor/model` rows, and the snapshot is refreshed from the same LiteLLM commit.

**Edit:** `crates/otel/tests/prices.rs`, `crates/otel/prices/builtin.json`, `crates/otel/src/context.rs` (one test only), `docs/spec/02-ingestion.md`.

**Steps.**

1. Replace `model_name` in `prices.rs` with:

   ```rust
   fn model_name(key: &str, provider: Option<&str>) -> Option<String> {
       let stripped = provider.and_then(|provider| key.strip_prefix(&format!("{provider}/")));
       let name = stripped.unwrap_or(key);
       if name.is_empty() || (stripped.is_none() && name.contains('/')) {
           None
       } else {
           Some(name.to_owned())
       }
   }
   ```

2. In `row_provider`, add the arm `"clf_ai_gateway" => "cloudflare_ai_gateway",` (before the catch-all).

3. Update the header comment (lines 1–22): the second bullet becomes "The model name is the LiteLLM key with a leading `<litellm_provider>/` removed; what is left may contain `/` (`groq/openai/gpt-oss-120b` gives `openai/gpt-oss-120b`, the id the client sends). A key without that prefix that contains `/` is skipped (D-IN-14)." Add `clf_ai_gateway` → `cloudflare_ai_gateway` where the comment describes `row_provider`.

4. In `conversion_follows_the_litellm_rules`, add to the input JSON:
   - `"novita/deepseek/deepseek-v3.2"` with `litellm_provider: "novita"`, `mode: "chat"`, `input_cost_per_token: 2.7e-7`, `output_cost_per_token: 4.1e-7`;
   - `"stray/name"` with `litellm_provider: "novita"`, `mode: "chat"`, `input_cost_per_token: 1e-6`.

   The existing `"openrouter/openai/gpt-4o"` entry now converts to the row `("openai/gpt-4o", Some("openrouter"))`. Update the expected sorted `(model_name, provider)` list with the rows the conversion now returns:
   - `("deepseek/deepseek-v3.2", Some("novita"))`;
   - `("openai/gpt-4o", Some("openrouter"))`;
   - no row for `stray/name`.

   Assert the novita row's `price_input` is `0.27` and `price_output` is `0.41`. Set the `collisions` assertion to the value the conversion returns, and state the old and new values in the phase report.

5. In `provider_mapping_covers_the_litellm_names`, add `("clf_ai_gateway", "cloudflare_ai_gateway")`.

6. Add `"openai/gpt-oss-120b"` and `"deepseek/deepseek-v3.2"` to `REQUIRED_MODELS` in `builtin_snapshot_is_valid`.

7. Download LiteLLM's file and refresh the snapshot:

   ```bash
   mkdir -p target/litellm
   curl -sSfL -o target/litellm/model_prices_and_context_window.json \
     https://raw.githubusercontent.com/BerriAI/litellm/4b498d1bba768bd81e4a434a14ea7ae2870dd01f/model_prices_and_context_window.json
   just prices-update target/litellm/model_prices_and_context_window.json
   ```

   Record the three numbers the refresh prints (`litellm price rows`, `collisions`, `bytes`) for the commit message.

8. In `crates/otel/src/context.rs` `mod tests`, add one test:

   ```rust
   #[test]
   fn every_snapshot_provider_is_a_fixed_point_of_rule_12() {
       for row in crate::cost::builtin_price_rows() {
           if let Some(provider) = &row.provider {
               assert_eq!(&canonical_provider(provider, false), provider, "{}", row.model_name);
           }
       }
   }
   ```

   If it fails for a provider name, stop and report the names. Do not change the alias table or the converter to make it pass.

9. Spec 02, the built-in prices paragraph (§ Cost computation, "Built-in prices: a snapshot of LiteLLM's …"): add that the model name keeps any `/` left after the provider prefix (D-IN-14), that `clf_ai_gateway` becomes `cloudflare_ai_gateway`, and that every snapshot provider is a fixed point of rule 12.

**Tests:** `conversion_follows_the_litellm_rules` and `provider_mapping_covers_the_litellm_names` (updated), `builtin_snapshot_is_valid` (with the new required models, and the compiled pattern set still within the 16 MiB limit), and `every_snapshot_provider_is_a_fixed_point_of_rule_12`.

**Done when:**

```bash
cargo nextest run -p cetra-otel --test prices
cargo nextest run -p cetra-otel --lib context::tests
cargo nextest run -p cetra-otel --test golden
```

All pass. `golden` covers `captured_llm_spans_are_priced_by_the_builtin_snapshot`. `git diff --stat crates/otel/prices/builtin.json` shows only additions, plus at most the renamed `clf_ai_gateway` rows.

**Commit:** `Keep LiteLLM vendor/model price rows in the snapshot`

Commit body:

```
Source: LiteLLM commit 4b498d1bba768bd81e4a434a14ea7ae2870dd01f. <rows> rows, <collisions> collisions, <bytes> bytes.
```

---

## IN1.4 — Capture harness: real hosts and suffix routes

**Goal:** a scenario turn can name a real host, the Python harness reaches the mock under that host, and the mock matches routes by path suffix.

**Edit:**
- `tools/capture/src/scenario.rs`, `tools/capture/src/mock.rs`, `tools/capture/src/write.rs`, `tools/capture/src/wire/openai.rs`, `tools/capture/src/wire/anthropic.rs`, `tools/capture/src/wire/gemini.rs`;
- `tools/capture/py/pyproject.toml`, `tools/capture/py/uv.lock`, `tools/capture/py/src/capture_py/harness.py`.

**Steps.**

1. `scenario.rs`: add to `Turn`, after `model`:

   ```rust
   /// The real host the program calls for this turn (D-IN-3); the mock checks the
   /// request's `Host` header against it. Absent: no check.
   #[serde(default)]
   pub host: Option<String>,
   ```

   In `check_turns`, reject a host that is not a lowercase DNS name with the message `turns[{i}].host must be a lowercase DNS name`. A valid name is 1–253 bytes long, consists only of `a-z`, `0-9`, `-` and `.`, contains at least one `.`, and neither starts nor ends with `.` or `-`. Add `host: None` to the `Turn` struct literals in `mock.rs` (tests), `write.rs`, `wire/openai.rs`, `wire/anthropic.rs` and `wire/gemini.rs`.

2. `mock.rs` routing:
   - Keep `HEAD /api/hello`.
   - Every other request goes to one handler. A `POST` whose path `classify` recognises is served as today. Anything else gets the existing unknown-route behaviour (404, and the error `cetra-capture mock: no route {METHOD} {path}`).
   - Replace `classify` with:

   ```rust
   fn classify(path: &str) -> Option<Route> {
       for (suffix, route) in [
           ("/chat/completions", Route::Chat),
           ("/responses", Route::Responses),
           ("/embeddings", Route::Embeddings),
           ("/messages", Route::Messages),
       ] {
           if path.ends_with(suffix) {
               return Some(route);
           }
       }
       let (prefix, action) = path.rsplit_once(':')?;
       let model = prefix.rsplit_once("/models/")?.1;
       if model.is_empty() || model.contains('/') {
           return None;
       }
       match action {
           "generateContent" => Some(Route::Generate),
           "streamGenerateContent" => Some(Route::GenerateStream),
           "embedContent" => Some(Route::Embed),
           "batchEmbedContents" => Some(Route::EmbedBatch),
           _ => None,
       }
   }
   ```

   `path` is the URI path, without the query.

3. `mock.rs` host check: after a turn is taken and its `api` matches the route, if `turn.host` is `Some(expected)`, compare it with the request's `Host` header, with any `:port` removed and lowercased. On a mismatch or a missing header:
   - the turn is consumed;
   - the error `cetra-capture mock: turn {index} expected host {expected}, got {got}` is logged (`got` is `-` when the header is missing);
   - the request is answered as an `api` mismatch is answered today.

   With `turn.host` absent, nothing is checked.

4. `pyproject.toml`:
   - Add `"httpx==<version>"` to `dependencies`, where `<version>` is the `httpx` version already in `tools/capture/py/uv.lock`.
   - Add `"cetra-sdk"` to `dependencies`, with `[tool.uv.sources]` `cetra-sdk = { path = "../../../sdks/python" }`.
   - Run `cd tools/capture/py && uv lock`. If the lock fails or changes any existing pin, stop and report the conflict.

5. `harness.py`: add

   ```python
   class RealHostTransport(httpx.BaseTransport):
       """Sends each request to the capture mock, keeping its path, query and headers (D-IN-3).

       The client keeps the real base URL, so the instrumentation records the real host,
       and the request's ``Host`` header still names it for the mock's check.
       """

       def __init__(self) -> None:
           self._mock = httpx.URL(mock_url())
           self._inner = httpx.HTTPTransport()

       def handle_request(self, request: httpx.Request) -> httpx.Response:
           request.url = request.url.copy_with(
               scheme=self._mock.scheme, host=self._mock.host, port=self._mock.port
           )
           return self._inner.handle_request(request)

       def close(self) -> None:
           self._inner.close()


   def real_host_client() -> httpx.Client:
       """An ``httpx.Client`` whose requests reach the mock under their real host."""
       return httpx.Client(transport=RealHostTransport())
   ```

   Also add `cetra_runtime(instrument: list[str]) -> TracerProvider`:
   - It imports `cetra._config.resolve_config`, `cetra._instrument.resolve` and `enable_for`, `cetra._runtime.Runtime` and `install`.
   - It builds `config = resolve_config(api_key="sk-cetra-capture", base_url="http://127.0.0.1:9", instrument=instrument, env={})`.
   - Then `classes = resolve(config.instrument)`, `runtime = Runtime.create(config, exporter=OTLPSpanExporter())`, `install(runtime)`, `enable_for(runtime, classes)`, and it returns `runtime.provider`.

   Its docstring says it uses cetra's private test seam, as `sdks/python/tests/conftest.py` does. That keeps the capture exporter uncompressed and unauthenticated, while the spans go through `CetraSpanProcessor`. The `base_url` is never contacted: the exporter is the harness's.

   The module passes ruff and mypy strict.

**Tests** (Rust, in the existing `mod tests`):
- `scenario.rs` `turn_host_must_be_a_lowercase_dns_name`: `openrouter.ai` and `api.novita.ai` pass. `OpenRouter.ai`, `openrouter.ai:443`, `https://openrouter.ai`, `localhost`, `.openrouter.ai`, `openrouter.ai.` and `""` fail with the message.
- `scenario.rs` `defaults_apply`: `host` defaults to `None`.
- `mock.rs` `each_route_matches_the_renderer`: add rows for `/api/v1/chat/completions` (Chat), `/openai/deployments/d/chat/completions?api-version=2024-10-21` (Chat), `/compatible-mode/v1/embeddings` (Embeddings), `/v1/projects/p/locations/l/publishers/google/models/gemini-2.5-flash:generateContent` (Generate).
- `mock.rs` `a_turn_host_must_match_the_request_host`: a turn with `host: Some("openrouter.ai")` is served for `Host: openrouter.ai` and for `Host: openrouter.ai:443`. A second scenario's turn is rejected and logged for `Host: 127.0.0.1:1234`, and for a request with no `Host` header.
- `mock.rs` `unknown_routes_are_404_and_logged`: add `/v1/chat/completions/extra` and `/models/a/b:generateContent`.

**Done when:**

```bash
cargo nextest run -p cetra-capture
cd tools/capture/py && uv sync --locked && uv run ruff format --check && uv run ruff check && uv run mypy
```

Both pass.

**Commit:** `Route real hosts to the capture mock and match routes by suffix`

---

## IN1.5 — SDK host stamp

**Goal:** with `instrument=["openai"]`, cetra's Python SDK sets `server.address` from the OpenAI client's base URL on spans started during a call.

**Edit:**
- `sdks/python/src/cetra/_host.py` (new), `sdks/python/src/cetra/_instrument.py`, `sdks/python/src/cetra/_processor.py`, `sdks/python/tests/test_instrument.py`;
- `docs/spec/10-sdk.md`, `website/src/content/docs/docs/reference/python/opentelemetry.mdx`.

**Steps.**

1. Create `_host.py`:

   ```python
   """The OpenAI client host stamp (D-IN-18, spec 10 § Instrumentors).

   OpenInference writes no endpoint on OpenAI spans, so the server cannot tell
   OpenRouter or Novita from OpenAI. While an OpenAI resource method runs, ``HOST``
   holds the client's base-URL host, and ``CetraSpanProcessor`` copies it into
   ``server.address`` on spans that start meanwhile.
   """

   HOST: ContextVar[str | None] = ContextVar("cetra_openai_host", default=None)

   # (module, class, method, is_async)
   _METHODS = (
       ("openai.resources.chat.completions", "Completions", "create", False),
       ("openai.resources.chat.completions", "AsyncCompletions", "create", True),
       ("openai.resources.responses", "Responses", "create", False),
       ("openai.resources.responses", "AsyncResponses", "create", True),
       ("openai.resources.embeddings", "Embeddings", "create", False),
       ("openai.resources.embeddings", "AsyncEmbeddings", "create", True),
   )


   class HostStamp:
       """Wraps the OpenAI resource methods; ``uninstrument`` restores them."""

       def instrument(self) -> None: ...
       def uninstrument(self) -> None: ...
   ```

   Behaviour:
   - **`instrument()`** imports each module with `importlib.import_module`. A module or class that is missing is skipped with one `logger.debug` line naming the module and class, and no payload. For each method found, it stores `(cls, name, original)` and sets `cls.<name>` to a wrapper made with `functools.wraps(original)`.
   - **The wrapper.** The sync wrapper is `token = HOST.set(_host_of(self))`, then `try: return original(self, *args, **kwargs)`, `finally: HOST.reset(token)`. The async wrapper is an `async def` doing the same with `return await original(...)`.
   - **`uninstrument()`** restores every stored original in reverse order and clears the list.
   - **Idempotent.** Calling `instrument()` twice wraps once, through a module-level `_installed` flag set by `instrument` and cleared by `uninstrument`.
   - **`_host_of(resource)`** returns `str(resource._client.base_url.host).lower()` or `None`. Any `Exception` gives `None`; it never raises.

   No new dependency: plain `functools`, no `wrapt`.

2. `_instrument.py` `enable_for`: after the loop has instrumented the instrumentor whose registry entry is `openai`, create `stamp = HostStamp()`, call `stamp.instrument()`, and append `stamp` to `runtime.instrumentors`. `Runtime.shutdown` already calls `uninstrument()` on each entry. Do it only when `openai` is among the enabled entries, and only once. An empty `instrument` list still imports nothing (the module docstring's rule): import `cetra._host` inside that branch.

3. `_processor.py` `_stamp`: at the end, add

   ```python
   host = HOST.get()
   if host is not None and "server.address" not in (span.attributes or {}):
       span.set_attribute("server.address", host)
   ```

   with `from cetra._host import HOST` at the top. `_host.py` imports only the standard library at module level, so this import stays cheap.

4. Spec 10 § Instrumentors: after the table, add a paragraph stating D-IN-18. When `instrument` names `openai`, the SDK wraps the OpenAI resource `create` methods (chat completions, responses, embeddings, sync and async) outermost. A context variable holds the client's base-URL host during the call, and the span processor sets `server.address` on spans that start during it and have none. The server's endpoint rule (spec 02 § Provider from the endpoint) then sets the provider. Shutdown restores the methods.

5. `website/.../reference/python/opentelemetry.mdx` § Instrumentation: add two sentences.
   - "With `instrument=["openai"]`, cetra records the host of the OpenAI client's base URL as `server.address`, so calls to OpenRouter, Novita or another provider behind the OpenAI client show that provider."
   - "The host is the only part of the URL that is recorded."

**Tests** (in `test_instrument.py`, using the `memory` and `spans` fixtures; the client uses `http_client=httpx.Client(transport=httpx.MockTransport(handler))`, where `handler` returns a canned chat completion, embedding or response JSON):
- `test_openai_span_records_the_client_host`: `start(instrument=["openai"])`, a client with `base_url="https://api.novita.ai/openai"`, one chat completion. The LLM span has `server.address == "api.novita.ai"`.
- `test_async_openai_span_records_the_client_host`: the same with `AsyncOpenAI` and `httpx.AsyncClient(transport=httpx.MockTransport(handler))`, run with `asyncio.run`.
- `test_streaming_span_records_the_client_host`: `stream=True` (the handler answers `text/event-stream` with two chunks and `[DONE]`). The span has `server.address`.
- `test_embeddings_span_records_the_client_host`: `client.embeddings.create(...)` with base URL `https://api.deepinfra.com/v1/openai` gives `api.deepinfra.com`.
- `test_host_is_not_stamped_outside_a_call`: a manual `cetra` span outside any OpenAI call has no `server.address`.
- `test_existing_server_address_is_kept`: inside `HOST.set("api.novita.ai")`, a span started with `attributes={"server.address": "custom"}` keeps `custom`.
- `test_no_stamp_without_the_openai_instrumentor`: `start(instrument=[])`. `openai.resources.chat.completions.Completions.create` is the original function.
- `test_shutdown_uninstruments`: extend it so that `Completions.create` and `AsyncCompletions.create` are the originals after `runtime_module.shutdown()`, and are not while running.

**Done when:**

```bash
cd sdks/python && uv sync --locked && uv run ruff format --check && uv run ruff check && uv run mypy && uv run pytest tests/test_instrument.py
```

All pass.

**Commit:** `Stamp the OpenAI client host on spans in the Python SDK`

---

## IN1.6 — Captured cases

**Goal:** four captured cases pin attribution and pricing for OpenRouter and Novita.

**Edit:**
- `tools/capture/py/scenarios/openinference/openrouter-chat/`, `…/openinference/novita-chat/`, `…/openllmetry/openrouter-chat/`, `…/openllmetry/novita-chat/` (each with `scenario.json`, `main.py`, and a module);
- the four matching `fixtures/otlp/<group>/<case>/` directories (written by `just capture` and `just fixtures-bless`, never by hand), and `fixtures/otlp/prices.json`;
- `docs/spec/02-ingestion.md` (§ Testing), `CHANGELOG.md`, `docs/roadmap.md`, `docs/mvp-decisions.md`.

**Steps.**

1. Each scenario is modelled on `openinference/openai-chat-tool`, with one chat turn and no tool.
   - The program builds `OpenAI(base_url=<real base URL>, api_key=os.environ["OPENAI_API_KEY"], max_retries=0, http_client=real_host_client())`.
   - It sends one `client.chat.completions.create(model=MODEL, messages=[{"role": "user", "content": "What is the capital of France?"}])`.
   - It ends with `finish(provider)`.

   | Case | Base URL | `MODEL` and turn `model` | Turn `host` | Tracing setup |
   |---|---|---|---|---|
   | `openinference/openrouter-chat` | `https://openrouter.ai/api/v1` | `openai/gpt-4o-mini` | `openrouter.ai` | `provider = cetra_runtime(["openai"])` |
   | `openinference/novita-chat` | `https://api.novita.ai/openai` | `deepseek/deepseek-v3.2` | `api.novita.ai` | `provider = cetra_runtime(["openai"])` |
   | `openllmetry/openrouter-chat` | `https://openrouter.ai/api/v1` | `openai/gpt-4o-mini` | `openrouter.ai` | `provider = setup()`, then OpenLLMetry's `OpenAIInstrumentor().instrument(tracer_provider=provider)`, env as in `openllmetry/openai-chat-tool` |
   | `openllmetry/novita-chat` | `https://api.novita.ai/openai` | `deepseek/deepseek-v3.2` | `api.novita.ai` | as above |

   - Turn: `api` `openai.chat`, `message.text` `"The capital of France is Paris."`, `finish` `stop`, `usage` `{input: 14, output: 8}`.
   - `spec_refs`: `02 § Model, provider, usage`, `02 § Provider from the endpoint`, `02 § Cost computation`.
   - `libraries`: the twin case's list. The OpenInference cases add `cetra-sdk`; `instrumentation` stays the instrumentor package.
   - `notes` say which attribute carries the host (`server.address` from cetra's stamp, or OpenLLMetry's `gen_ai.openai.api_base`) and what the provider and cost resolve to.

2. Add a row to `fixtures/otlp/prices.json` (canonical format; `just fixtures-fmt`): `model_name` `deepseek/deepseek-v3.2`, `match_pattern` `deepseek/deepseek\-v3\.2`, `provider` `novita`, `valid_from` `1970-01-01T00:00:00.000000000Z`, `price_input` 0.27, `price_output` 0.41, the other prices and `tiers` null. The OpenRouter cases are priced by the existing `gpt-4o-mini` row through D-IN-13.

3. Capture and bless:

   ```bash
   just capture openinference/openrouter-chat openinference/novita-chat openllmetry/openrouter-chat openllmetry/novita-chat
   just fixtures-bless
   ```

   Check each `expected.json` by hand:
   - the LLM span's `provider` is `openrouter` (both OpenRouter cases) or `novita` (both Novita cases);
   - `cost_source` is `computed`;
   - the OpenRouter cost equals the OpenAI `gpt-4o-mini` price for 14 input and 8 output tokens;
   - `server.address` (OpenInference cases) or `gen_ai.openai.api_base` (OpenLLMetry cases) is still in `attributes`.

   If any of these does not hold, stop and report the `expected.json` excerpt. Do not edit fixtures by hand.

4. Spec 02 § Testing:
   - In the bullet about captured cases, add that a turn may name a real host, which the harness routes to the mock (D-IN-3), and that the mock matches routes by path suffix (D-IN-4).
   - Add a row or a note in the matrix table: "OpenAI client to OpenRouter and Novita: OpenInference through cetra's SDK, OpenLLMetry (D-IN-19)".

5. `CHANGELOG.md` `[Unreleased]`:
   - Under `### Added` (create the heading): "Calls through the OpenAI client to OpenRouter, Groq, Novita, Together, Fireworks, DeepSeek, xAI, Databricks and other providers show that provider, read from the endpoint's host; the Python SDK records the OpenAI client's host for this."
   - Under `### Fixed`: "OpenRouter, Vercel AI Gateway, Helicone, Portkey and Cloudflare AI Gateway spans are priced by the model publisher's price, and the built-in prices include Groq, Novita, Nebius, DeepInfra, Baseten, Hyperbolic, watsonx and Together models that were missing."

6. `docs/roadmap.md`, IN row: Plan cell adds "M1: [`01-m1-provider-attribution.md`](plans/in-integrations/01-m1-provider-attribution.md)". Status becomes "**M1 implemented** (provider from the endpoint, gateway pricing, snapshot fix, capture host routing, SDK host stamp)."

7. `docs/mvp-decisions.md` § Track IN: add D-IN-19 with the text of this plan's decision table row.

**Tests:** the golden test covers the four new cases, and `captured_llm_spans_are_priced_by_the_builtin_snapshot` checks them against the refreshed snapshot. The fixtures test checks `prices.json` and the cases.

**Done when:**

```bash
cargo nextest run -p cetra-otel --test golden --test fixtures
cargo run -q -p cetra-capture -- run openinference/openrouter-chat openinference/novita-chat openllmetry/openrouter-chat openllmetry/novita-chat --check
```

Both pass. (The capture check re-runs the four scenarios and compares them.)

**Commit:** `Capture OpenRouter and Novita through OpenInference and OpenLLMetry`

---

## After the last phase

The main session runs `just check` once (with Node on `PATH`), then a Sonnet code review of M1, fixes its findings through a Haiku agent, and pushes to PR #242.
