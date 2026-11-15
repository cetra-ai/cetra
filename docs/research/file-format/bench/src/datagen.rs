//! Synthetic but LLM-shaped spans: Zipf-distributed vocabulary, per-app system
//! prompts and tool definitions, conversations that grow within a trace, RAG
//! chunks drawn from a shared corpus, attribute maps with realistic keys.

use std::collections::BTreeMap;

use llmobs_core::enums::{Convention, CostSource, PayloadFormat, SpanKind, SpanStatus};
use llmobs_core::ids::{ProjectId, SpanId, TraceId};
use llmobs_core::span::{Span, SpanEvent};
use serde_json::json;

pub const NS: i64 = 1_000_000_000;
pub const DAY_START: i64 = 1_772_409_600 * NS; // 2026-03-02T00:00:00Z
const DAY: i64 = 86_400 * NS;

pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }
    pub fn lognormal(&mut self, median: f64, p99: f64) -> f64 {
        let u1 = self.unit().max(1e-12);
        let u2 = self.unit();
        let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
        let sigma = (p99 / median).ln() / 2.326;
        median * (sigma * z).exp()
    }
}

/// Zipf vocabulary of pronounceable pseudo-words.
pub struct Vocab {
    words: Vec<String>,
    cdf: Vec<f64>,
}
impl Vocab {
    fn new(rng: &mut Rng, n: usize) -> Self {
        const SYL: [&str; 40] = [
            "ta", "re", "mi", "on", "ka", "lo", "si", "en", "da", "pu", "ve", "ri", "no", "be",
            "tu", "an", "ce", "go", "li", "ma", "st", "er", "in", "th", "ou", "al", "ex", "co",
            "de", "pro", "ing", "tion", "ment", "able", "ly", "ed", "un", "pre", "sub", "ize",
        ];
        let mut words = Vec::with_capacity(n);
        for i in 0..n {
            let syl = if i < 100 { 1 + rng.below(2) } else { 2 + rng.below(3) };
            let mut w = String::new();
            for _ in 0..syl {
                w.push_str(SYL[rng.below(SYL.len())]);
            }
            words.push(w);
        }
        let mut cdf = Vec::with_capacity(n);
        let mut acc = 0.0;
        for r in 0..n {
            acc += 1.0 / (r as f64 + 2.7).powf(1.07);
            cdf.push(acc);
        }
        for c in &mut cdf {
            *c /= acc;
        }
        Self { words, cdf }
    }
    fn word(&self, rng: &mut Rng) -> &str {
        let u = rng.unit();
        let i = self.cdf.partition_point(|&c| c < u).min(self.words.len() - 1);
        &self.words[i]
    }
    pub fn text(&self, rng: &mut Rng, words: usize) -> String {
        let mut s = String::with_capacity(words * 7);
        let mut sentence = 0;
        for i in 0..words {
            if i > 0 {
                s.push(' ');
            }
            let w = self.word(rng);
            if sentence == 0 {
                let mut c = w.chars();
                if let Some(f) = c.next() {
                    s.extend(f.to_uppercase());
                    s.push_str(c.as_str());
                }
            } else {
                s.push_str(w);
            }
            sentence += 1;
            if sentence > 6 && rng.unit() < 0.12 {
                s.push(if rng.unit() < 0.9 { '.' } else { '?' });
                sentence = 0;
                if rng.unit() < 0.1 {
                    s.push('\n');
                }
            } else if rng.unit() < 0.04 {
                s.push(',');
            }
        }
        s.push('.');
        s
    }
}

struct App {
    name: String,
    system: String,
    tools: Option<String>,
    tool_names: Vec<String>,
    service: usize,
    model: usize,
}

pub struct World {
    vocab: Vocab,
    apps: Vec<App>,
    corpus: Vec<String>,
    project: ProjectId,
}

const MODELS: [(&str, &str); 6] = [
    ("openai", "gpt-4o"),
    ("openai", "gpt-4o-mini"),
    ("anthropic", "claude-3-5-sonnet"),
    ("anthropic", "claude-3-haiku"),
    ("aws.bedrock", "llama-3-70b"),
    ("gcp.gemini", "gemini-1.5-pro"),
];
const SERVICES: [&str; 8] = [
    "chat-api", "agent-worker", "rag-service", "support-bot", "batch-eval", "search-api",
    "summarizer", "router",
];
const TAGS: [&str; 10] = [
    "beta", "canary", "internal", "external", "gpu", "cpu", "batch", "realtime", "prod", "test",
];
const RELEASES: [&str; 5] = ["v1.0.3", "v1.1.0", "v1.2.7", "v2.0.0", "v2.1.4"];

impl World {
    pub fn new(seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let vocab = Vocab::new(&mut rng, 6000);
        let mut apps = Vec::new();
        for a in 0..40 {
            let system = ({ let n = rng.range(300, 2200); vocab.text(&mut rng, n) });
            let ntools = rng.below(7);
            let mut tool_names = Vec::new();
            let tools = (ntools > 0).then(|| {
                let defs: Vec<_> = (0..ntools)
                    .map(|t| {
                        let name = format!("{}_{}", vocab.word(&mut rng), t);
                        tool_names.push(name.clone());
                        let desc = ({ let n = rng.range(20, 80); vocab.text(&mut rng, n) });
                        json!({"type":"function","name":name,"description":desc,
                          "parameters":{"type":"object","properties":{
                            "query":{"type":"string","description":vocab.text(&mut rng, 8)},
                            "limit":{"type":"integer","minimum":1,"maximum":50}},
                          "required":["query"]}})
                    })
                    .collect();
                serde_json::to_string(&defs).unwrap_or_default()
            });
            apps.push(App {
                name: format!("app-{a}-{}", vocab.word(&mut rng)),
                system,
                tools,
                tool_names,
                service: rng.below(SERVICES.len()),
                model: rng.below(MODELS.len()),
            });
        }
        let corpus = (0..3000).map(|_| ({ let n = rng.range(80, 400); vocab.text(&mut rng, n) })).collect();
        Self {
            vocab,
            apps,
            corpus,
            project: "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap(),
        }
    }

    /// One trace's spans.
    pub fn trace(&self, rng: &mut Rng, out: &mut Vec<Span>) {
        let app = &self.apps[rng.below(self.apps.len())];
        let mut tid = [0u8; 16];
        tid[..8].copy_from_slice(&rng.next().to_le_bytes());
        tid[8..].copy_from_slice(&rng.next().to_le_bytes());
        tid[0] |= 1;
        let trace_id = TraceId::from_bytes(tid).unwrap();
        // Diurnal start time over one day.
        let start = loop {
            let t = rng.unit();
            let hour = t * 24.0;
            let w = if (8.0..20.0).contains(&hour) { 1.0 } else { 0.5 };
            if rng.unit() < w {
                break DAY_START + (t * DAY as f64) as i64;
            }
        };
        let session = (rng.unit() < 0.3).then(|| format!("sess-{:08x}", rng.below(50_000)));
        let user = (rng.unit() < 0.6).then(|| format!("user-{}", rng.below(20_000)));
        let env = if rng.unit() < 0.8 { "production" } else { "staging" };
        let release = RELEASES[rng.below(RELEASES.len())];
        let customer = rng.below(500);
        let has_error = rng.unit() < 0.03;
        let mut sid: u64 = rng.next() | 1;
        let mut next_sid = || {
            sid = sid.wrapping_add(0x9E37_79B9);
            SpanId::from_bytes(sid.to_be_bytes()).unwrap()
        };
        let root_id = next_sid();
        let ingest = start + 30 * NS;
        let resource: BTreeMap<String, String> = [
            ("telemetry.sdk.name", "\"opentelemetry\"".to_owned()),
            ("telemetry.sdk.language", "\"python\"".to_owned()),
            ("telemetry.sdk.version", "\"1.27.0\"".to_owned()),
            ("host.name", format!("\"{}-{}\"", SERVICES[app.service], app.service * 7 + 3)),
            ("process.pid", format!("{}", 1000 + app.service * 13)),
            ("deployment.environment", format!("\"{env}\"")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let mut metadata = BTreeMap::new();
        metadata.insert("customer_id".to_owned(), format!("\"cust-{customer}\""));
        if rng.unit() < 0.5 {
            metadata.insert("plan".to_owned(), ["\"free\"", "\"pro\"", "\"team\""][rng.below(3)].to_owned());
        }
        let mut tags = Vec::new();
        for _ in 0..rng.below(4) {
            let t = TAGS[rng.below(TAGS.len())].to_owned();
            if !tags.contains(&t) {
                tags.push(t);
            }
        }
        tags.sort();

        let base = |span_id: SpanId, parent: Option<SpanId>, name: String, kind: SpanKind, s: i64, e: i64| Span {
            project_id: self.project,
            trace_id,
            span_id,
            parent_span_id: parent,
            name,
            span_kind: kind,
            start_time: s,
            end_time: e,
            status: SpanStatus::Unset,
            status_message: None,
            service_name: Some(SERVICES[app.service].to_owned()),
            session_id: session.clone(),
            user_id: user.clone(),
            environment: Some(env.to_owned()),
            release: Some(release.to_owned()),
            tags: tags.clone(),
            metadata: metadata.clone(),
            provider: None,
            request_model: None,
            response_model: None,
            operation: None,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            total_tokens: None,
            cost_input: None,
            cost_output: None,
            cost_total: None,
            cost_source: None,
            ttft_ns: None,
            request_params: None,
            input: None,
            input_format: None,
            output: None,
            output_format: None,
            input_preview: None,
            output_preview: None,
            system_instructions: None,
            tool_definitions: None,
            attributes: BTreeMap::new(),
            resource_attributes: resource.clone(),
            scope_name: Some("openinference.instrumentation.openai".to_owned()),
            scope_version: Some("0.1.18".to_owned()),
            events: Vec::new(),
            links: Vec::new(),
            media_refs: Vec::new(),
            truncated: Vec::new(),
            conventions: vec![Convention::Openinference],
            ingest_time: ingest,
            schema_version: 1,
        };

        let question = ({ let n = rng.range(8, 150); self.vocab.text(rng, n) });
        let mut messages: Vec<serde_json::Value> =
            vec![json!({"role":"user","parts":[{"type":"text","content":question}]})];
        let first = out.len();
        out.push(base(root_id, None, app.name.clone(), SpanKind::Agent, start, start));
        let n_llm = 1 + rng.below(5);
        let mut t = start + (rng.unit() * 0.2 * NS as f64) as i64;
        let mut final_answer = String::new();
        let (provider, model) = MODELS[app.model];
        for i in 0..n_llm {
            let dur = (rng.lognormal(1.5, 20.0) * NS as f64) as i64;
            let id = next_sid();
            let mut s = base(id, Some(root_id), format!("chat {model}"), SpanKind::Llm, t, t + dur);
            let input = serde_json::to_string(&messages).unwrap_or_default();
            let in_tokens = (input.len() + app.system.len()) as i64 / 4;
            let call_tool = i + 1 < n_llm && !app.tool_names.is_empty() && rng.unit() < 0.7;
            let answer = ({ let n = rng.range(20, 450); self.vocab.text(rng, n) });
            let out_msg = if call_tool {
                let tool = &app.tool_names[rng.below(app.tool_names.len())];
                json!([{"role":"assistant","parts":[
                    {"type":"text","content":({ let n = rng.range(5, 30); self.vocab.text(rng, n) })},
                    {"type":"tool_call","id":format!("call_{:x}", rng.next()),"name":tool,
                     "arguments":{"query":({ let n = rng.range(3, 12); self.vocab.text(rng, n) }),"limit":5}}],
                    "finish_reason":"tool_calls"}])
            } else {
                json!([{"role":"assistant","parts":[{"type":"text","content":answer}],"finish_reason":"stop"}])
            };
            let output = serde_json::to_string(&out_msg).unwrap_or_default();
            let out_tokens = output.len() as i64 / 4;
            s.provider = Some(provider.to_owned());
            s.request_model = Some(model.to_owned());
            s.response_model = Some(format!("{model}-2024-08-06"));
            s.operation = Some("chat".to_owned());
            s.input_tokens = Some(in_tokens);
            s.output_tokens = Some(out_tokens);
            s.cache_read_tokens = (rng.unit() < 0.4).then(|| app.system.len() as i64 / 4);
            s.total_tokens = Some(in_tokens + out_tokens);
            s.cost_input = Some(in_tokens as f64 * 2.5e-6);
            s.cost_output = Some(out_tokens as f64 * 1e-5);
            s.cost_total = Some(in_tokens as f64 * 2.5e-6 + out_tokens as f64 * 1e-5);
            s.cost_source = Some(CostSource::Computed);
            s.ttft_ns = Some(dur / 5);
            s.request_params = Some(format!(
                "{{\"temperature\":{:.1},\"max_tokens\":{},\"top_p\":1.0}}",
                rng.below(11) as f64 / 10.0,
                [256, 1024, 4096][rng.below(3)]
            ));
            s.input_preview = Some(preview(&question));
            s.output_preview = Some(preview(&answer));
            s.input = Some(input);
            s.input_format = Some(PayloadFormat::Messages);
            s.output = Some(output);
            s.output_format = Some(PayloadFormat::Messages);
            s.system_instructions = Some(serde_json::to_string(&json!([{"type":"text","content":app.system}])).unwrap_or_default());
            s.tool_definitions = app.tools.clone();
            s.attributes = llm_attributes(rng, model);
            messages.extend(out_msg.as_array().cloned().unwrap_or_default());
            out.push(s);
            t += dur;
            if call_tool {
                let tdur = (rng.lognormal(0.3, 5.0) * NS as f64) as i64;
                let tool = app.tool_names[rng.below(app.tool_names.len())].clone();
                let mut ts = base(next_sid(), Some(root_id), tool, SpanKind::Tool, t, t + tdur);
                let chunks: Vec<&str> =
                    (0..rng.range(1, 4)).map(|_| self.corpus[zipf_doc(rng, self.corpus.len())].as_str()).collect();
                let result = chunks.join("\n\n");
                ts.input = Some(format!("{{\"query\":\"{}\",\"limit\":5}}", ({ let n = rng.range(3, 12); self.vocab.text(rng, n) })));
                ts.input_format = Some(PayloadFormat::Json);
                ts.output = Some(serde_json::to_string(&result).unwrap_or_default());
                ts.output_format = Some(PayloadFormat::Text);
                ts.input_preview = ts.input.as_deref().map(preview);
                ts.output_preview = Some(preview(&result));
                ts.attributes.insert("tool.call_id".to_owned(), format!("\"call_{:x}\"", rng.next()));
                messages.push(json!({"role":"tool","parts":[{"type":"tool_call_response","id":"call","response":result}]}));
                out.push(ts);
                t += tdur;
            } else {
                final_answer = answer;
            }
        }
        let end = t + NS / 50;
        let root = &mut out[first];
        root.end_time = end;
        root.input = Some(question.clone());
        root.input_format = Some(PayloadFormat::Text);
        root.input_preview = Some(preview(&question));
        root.output = Some(final_answer.clone());
        root.output_format = Some(PayloadFormat::Text);
        root.output_preview = Some(preview(&final_answer));
        if has_error {
            let k = first + 1 + rng.below(out.len() - first - 1);
            let s = &mut out[k];
            s.status = SpanStatus::Error;
            s.status_message = Some("RateLimitError: Error code: 429".to_owned());
            s.events.push(SpanEvent {
                time: s.end_time,
                name: "exception".to_owned(),
                attributes: Some(format!(
                    "{{\"exception.type\":\"RateLimitError\",\"exception.stacktrace\":\"{}\"}}",
                    self.vocab.text(rng, 60)
                )),
            });
        }
    }
}

fn zipf_doc(rng: &mut Rng, n: usize) -> usize {
    // Popular documents are retrieved far more often.
    let u = rng.unit();
    ((u * u * u) * n as f64) as usize % n
}

fn preview(s: &str) -> String {
    s.chars().take(512).collect()
}

fn llm_attributes(rng: &mut Rng, model: &str) -> BTreeMap<String, String> {
    let mut a = BTreeMap::new();
    a.insert("llm.system".to_owned(), "\"openai\"".to_owned());
    a.insert("llm.model_name".to_owned(), format!("\"{model}\""));
    a.insert("openinference.span.kind".to_owned(), "\"LLM\"".to_owned());
    a.insert("http.status_code".to_owned(), "200".to_owned());
    a.insert("gen_ai.response.id".to_owned(), format!("\"chatcmpl-{:x}{:x}\"", rng.next(), rng.next()));
    if rng.unit() < 0.5 {
        a.insert("llm.invocation_parameters".to_owned(), format!(
            "\"{{\\\"stream\\\": {}, \\\"seed\\\": {}}}\"",
            rng.unit() < 0.5,
            rng.below(1000)
        ));
    }
    if rng.unit() < 0.3 {
        a.insert("thread.id".to_owned(), format!("{}", 139_000_000 + rng.below(1000)));
    }
    a
}
