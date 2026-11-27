use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use arrow::array::{Array, AsArray, StructArray};
use flate2::read::GzDecoder;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use serde_json::json;

use crate::schema::{
    BenchResult, ScoreRow, SpanEventRow, SpanLinkRow, SpanRow, sort_scores, sort_spans,
};

pub(crate) const NS: i64 = 1_000_000_000;
pub(crate) const DAY_START: i64 = 1_772_409_600 * NS; // 2026-03-02T00:00:00Z
pub(crate) const HOUR_NS: i64 = 3_600 * NS;
pub(crate) const DAY_NS: i64 = 24 * HOUR_NS;
pub(crate) const RARE_NEEDLE: &str = "zqxj_rare_needle";
pub(crate) const ABSENT_NEEDLE: &str = "never_present_xyz987";

const PROJECT_ID: &str = "018f6b4e-7c1a-7c1a-8c1a-000000000001";

const MODELS: [(&str, &str); 6] = [
    ("openai", "gpt-4o"),
    ("openai", "gpt-4o-mini"),
    ("anthropic", "claude-3-5-sonnet"),
    ("anthropic", "claude-3-haiku"),
    ("aws.bedrock", "llama-3-70b"),
    ("gcp.gemini", "gemini-1.5-pro"),
];

const SERVICES: [&str; 6] = [
    "chat-api",
    "agent-worker",
    "rag-service",
    "support-bot",
    "code-agent",
    "eval-runner",
];

const ENVS: [&str; 2] = ["production", "staging"];
const RELEASES: [&str; 4] = ["v1.0.3", "v1.1.0", "v2.0.0", "v2.1.4"];
const TAGS: [&str; 8] = [
    "prod",
    "streaming",
    "gpu",
    "agent",
    "code",
    "chat",
    "canary",
    "batch",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BenchScale {
    Quick,
    Full,
}

impl BenchScale {
    pub(crate) fn from_env() -> Self {
        match std::env::var("STATAM_BENCH_SCALE").as_deref() {
            Ok("quick") | Ok("QUICK") => Self::Quick,
            _ => Self::Full,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Full => "full",
        }
    }
}

pub(crate) struct WorkloadData {
    pub(crate) w1_spans: Vec<SpanRow>,
    pub(crate) w2_slices: Vec<Vec<SpanRow>>,
    pub(crate) w3_spans: Vec<SpanRow>,
    pub(crate) w3_scores: Vec<ScoreRow>,
    pub(crate) w4_spans: Vec<SpanRow>,
    pub(crate) w4_tiny_flushes: Vec<Vec<SpanRow>>,
}

pub(crate) fn generate_all_workloads(scale: BenchScale) -> BenchResult<WorkloadData> {
    let (w1_target, w2_per_slice, w3_spans_n, w3_scores_n, w4_n) = match scale {
        BenchScale::Quick => (1_800usize, 450usize, 25_000usize, 12_000usize, 1_500usize),
        BenchScale::Full => (
            20_000usize,
            5_000usize,
            250_000usize,
            125_000usize,
            10_000usize,
        ),
    };

    let mut w1_spans = if let Ok(dir) = std::env::var("STATAM_SWE_DIR") {
        load_swe_trajectories(Path::new(&dir), w1_target)?
    } else {
        generate_w1_agent_trajectories(w1_target)
    };
    plant_rare_needle(&mut w1_spans, 12);
    sort_spans(&mut w1_spans);

    let mut w2_slices = if let Ok(path) = std::env::var("STATAM_OASST2") {
        load_oasst2_slices(Path::new(&path), w2_per_slice)?
    } else {
        generate_w2_chat_slices(w2_per_slice)
    };
    for (idx, slice) in w2_slices.iter_mut().enumerate() {
        if idx == 1 || idx == 3 {
            plant_rare_needle(slice, 4);
        }
        sort_spans(slice);
    }

    let mut w3_spans = generate_w3_narrow_spans(w3_spans_n);
    sort_spans(&mut w3_spans);

    let mut w3_scores = generate_w3_scores(w3_scores_n);
    sort_scores(&mut w3_scores);

    let mut w4_spans = generate_w4_adversarial(w4_n);
    plant_rare_needle(&mut w4_spans, 6);
    sort_spans(&mut w4_spans);

    let w4_tiny_flushes = generate_w4_tiny_flushes(20);

    Ok(WorkloadData {
        w1_spans,
        w2_slices,
        w3_spans,
        w3_scores,
        w4_spans,
        w4_tiny_flushes,
    })
}

fn make_trace_id(rng: &mut SmallRng) -> [u8; 16] {
    let mut tid = [0u8; 16];
    rng.fill(&mut tid);
    tid[0] |= 1;
    tid
}

fn make_span_id(rng: &mut SmallRng) -> [u8; 8] {
    let mut sid = [0u8; 8];
    rng.fill(&mut sid);
    sid[0] |= 1;
    sid
}

fn base_span(
    rng: &mut SmallRng,
    trace_id: [u8; 16],
    span_id: [u8; 8],
    parent_span_id: Option<[u8; 8]>,
    name: String,
    span_kind: &str,
    start_time: i64,
    end_time: i64,
) -> SpanRow {
    let svc = SERVICES[rng.random_range(0..SERVICES.len())];
    let env = ENVS[usize::from(rng.random_bool(0.2))];
    let rel = RELEASES[rng.random_range(0..RELEASES.len())];
    let cust = if rng.random_bool(0.02) {
        42
    } else {
        rng.random_range(0..500)
    };
    let mut metadata = BTreeMap::new();
    metadata.insert("customer_id".to_owned(), Some(format!("cust-{cust}")));
    if rng.random_bool(0.5) {
        let plan = ["free", "pro", "team"][rng.random_range(0..3)];
        metadata.insert("plan".to_owned(), Some(plan.to_owned()));
    }

    let mut resource = BTreeMap::new();
    resource.insert(
        "telemetry.sdk.name".to_owned(),
        Some("opentelemetry".to_owned()),
    );
    resource.insert("deployment.environment".to_owned(), Some(env.to_owned()));
    resource.insert("service.name".to_owned(), Some(svc.to_owned()));

    let mut tags = Vec::new();
    let ntags = rng.random_range(1..=3);
    for _ in 0..ntags {
        let t = TAGS[rng.random_range(0..TAGS.len())].to_owned();
        if !tags.contains(&t) {
            tags.push(t);
        }
    }

    SpanRow {
        project_id: PROJECT_ID.to_owned(),
        trace_id,
        span_id,
        parent_span_id,
        name,
        span_kind: span_kind.to_owned(),
        start_time,
        end_time,
        duration_ns: end_time.saturating_sub(start_time),
        status: "ok".to_owned(),
        status_message: None,
        service_name: Some(svc.to_owned()),
        session_id: Some(format!("sess-{:06}", rng.random_range(0..5_000))),
        user_id: Some(format!("user-{}", rng.random_range(0..2_000))),
        environment: Some(env.to_owned()),
        release: Some(rel.to_owned()),
        tags,
        metadata,
        provider: None,
        request_model: None,
        response_model: None,
        model: None,
        prompt_name: None,
        prompt_version: None,
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
        resource_attributes: resource,
        scope_name: Some("openinference.instrumentation".to_owned()),
        scope_version: Some("0.1.18".to_owned()),
        events: Vec::new(),
        links: Vec::new(),
        media_refs: Vec::new(),
        truncated: Vec::new(),
        conventions: vec!["openinference".to_owned()],
        ingest_time: end_time + 2 * NS,
        schema_version: 1,
    }
}

fn preview(s: &str) -> String {
    s.chars().take(256).collect()
}

fn pseudo_code_diff(rng: &mut SmallRng, lines: usize) -> String {
    const SNIPPETS: [&str; 12] = [
        "@@ -42,7 +42,11 @@ fn resolve_symbol(ctx: &Context, id: SymbolId) -> Option<Type>",
        "+    if let Some(cached) = ctx.cache.get(&id) {",
        "+        return Some(cached.clone());",
        "+    }",
        "     let entry = ctx.table.lookup(id)?;",
        "-    let normalized = entry.ty.flatten();",
        "+    let normalized = entry.ty.normalize_with_bounds(&ctx.bounds);",
        "     ctx.diagnostics.record_resolution(id, &normalized);",
        "     Some(normalized)",
        "diff --git a/src/solver/infer.rs b/src/solver/infer.rs",
        "index 8a91c3e..f02b71a 100644",
        "--- a/src/solver/infer.rs\n+++ b/src/solver/infer.rs",
    ];
    let mut out = String::with_capacity(lines * 64);
    for i in 0..lines {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(SNIPPETS[rng.random_range(0..SNIPPETS.len())]);
    }
    out
}

fn plant_rare_needle(rows: &mut [SpanRow], count: usize) {
    if rows.is_empty() {
        return;
    }
    let step = (rows.len() / count.max(1)).max(1);
    let mut planted = 0usize;
    let mut idx = 3.min(rows.len() - 1);
    while planted < count && idx < rows.len() {
        if let Some(inp) = rows[idx].input.as_mut() {
            if let Some(pos) = inp.find("\"content\":\"") {
                inp.insert_str(pos + 11, &format!("{RARE_NEEDLE} "));
            } else {
                inp.push_str(&format!(" {RARE_NEEDLE}"));
            }
            planted += 1;
        }
        idx += step;
    }
    if planted == 0 {
        let take_n = count.min(rows.len());
        for r in rows.iter_mut().take(take_n) {
            r.input = Some(format!("payload containing {RARE_NEEDLE} marker"));
        }
    }
}

fn generate_w1_agent_trajectories(target_rows: usize) -> Vec<SpanRow> {
    let mut rng = SmallRng::seed_from_u64(0x5731_0001);
    let system_prompts: Vec<String> = (0..12)
        .map(|i| {
            format!(
                "You are an autonomous SWE coding agent working on repository repo-{i}. \
                 Follow strict test-driven changes: inspect files via search_code, read_file, edit_file, \
                 and run cargo test/pytest before declaring success. Never fabricate outputs. \
                 Always preserve invariants, thread safety, and bounds checks across module {i}."
            )
            .repeat(8)
        })
        .collect();
    let tool_defs = serde_json::to_string(&json!([
        {"type":"function","name":"search_code","parameters":{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}}}},
        {"type":"function","name":"read_file","parameters":{"type":"object","properties":{"path":{"type":"string"},"start":{"type":"integer"},"end":{"type":"integer"}}}},
        {"type":"function","name":"apply_patch","parameters":{"type":"object","properties":{"diff":{"type":"string"}}}},
        {"type":"function","name":"run_tests","parameters":{"type":"object","properties":{"filter":{"type":"string"}}}}
    ]))
    .unwrap_or_default();

    let mut rows = Vec::with_capacity(target_rows + 64);
    let mut traj_idx = 0usize;
    while rows.len() < target_rows {
        let tid = make_trace_id(&mut rng);
        let root_sid = make_span_id(&mut rng);
        let repo_id = traj_idx % system_prompts.len();
        let sys_json =
            serde_json::to_string(&json!([{"type":"text","content":system_prompts[repo_id]}]))
                .unwrap_or_default();
        let (provider, model) = MODELS[rng.random_range(0..MODELS.len())];
        let t_offset = rng.random_range(0..DAY_NS);
        let mut t = DAY_START + t_offset;
        let turns = rng.random_range(10..=26);
        let mut history: Vec<serde_json::Value> = vec![json!({
            "role": "user",
            "parts": [{"type": "text", "content": format!("Fix issue #{}: Type mismatch in module_{repo_id} during incremental compile.", 1000 + traj_idx)}]
        })];

        let root_idx = rows.len();
        rows.push(base_span(
            &mut rng,
            tid,
            root_sid,
            None,
            format!("swe-agent-repo-{repo_id}"),
            "agent",
            t,
            t + NS,
        ));

        for turn in 0..turns {
            if rows.len() >= target_rows {
                break;
            }
            let dur = rng.random_range(200..4_000) * 1_000_000;
            let sid = make_span_id(&mut rng);
            let mut s = base_span(
                &mut rng,
                tid,
                sid,
                Some(root_sid),
                format!("chat {model}"),
                "llm",
                t,
                t + dur,
            );
            let input_json = serde_json::to_string(&history).unwrap_or_default();
            let diff_lines = rng.random_range(12..36);
            let diff = pseudo_code_diff(&mut rng, diff_lines);
            let call_tool = turn + 1 < turns;
            let out_val = if call_tool {
                let tool_name = ["search_code", "read_file", "apply_patch", "run_tests"]
                    [rng.random_range(0..4)];
                json!([{
                    "role": "assistant",
                    "parts": [
                        {"type": "text", "content": format!("Step {turn}: inspecting symbol table and patching.")},
                        {"type": "tool_call", "id": format!("call_{traj_idx}_{turn}"), "name": tool_name, "arguments": {"diff": diff}}
                    ],
                    "finish_reason": "tool_calls"
                }])
            } else {
                json!([{
                    "role": "assistant",
                    "parts": [{"type": "text", "content": format!("All tests pass after patching module_{repo_id}.\n{diff}")}],
                    "finish_reason": "stop"
                }])
            };
            let output_json = serde_json::to_string(&out_val).unwrap_or_default();
            let in_tok = (input_json.len() / 4) as i64;
            let out_tok = (output_json.len() / 4) as i64;
            s.provider = Some(provider.to_owned());
            s.request_model = Some(model.to_owned());
            s.response_model = Some(format!("{model}-2025"));
            s.model = Some(model.to_owned());
            s.operation = Some("chat".to_owned());
            s.input_tokens = Some(in_tok);
            s.output_tokens = Some(out_tok);
            s.total_tokens = Some(in_tok + out_tok);
            s.cost_input = Some(in_tok as f64 * 2.5e-6);
            s.cost_output = Some(out_tok as f64 * 1.0e-5);
            s.cost_total = Some(in_tok as f64 * 2.5e-6 + out_tok as f64 * 1.0e-5);
            s.cost_source = Some("computed".to_owned());
            s.ttft_ns = Some(dur / 4);
            s.input = Some(input_json);
            s.input_format = Some("messages".to_owned());
            s.output = Some(output_json);
            s.output_format = Some("messages".to_owned());
            s.input_preview = s.input.as_deref().map(preview);
            s.output_preview = s.output.as_deref().map(preview);
            s.system_instructions = Some(sys_json.clone());
            s.tool_definitions = Some(tool_defs.clone());
            if rng.random_bool(0.03) {
                s.status = "error".to_owned();
                s.status_message = Some("ToolTimeoutError: test runner exceeded 30s".to_owned());
            }
            s.attributes
                .insert("http.status_code".to_owned(), Some("200".to_owned()));
            s.attributes
                .insert("llm.model_name".to_owned(), Some(model.to_owned()));
            if let Some(arr) = out_val.as_array() {
                history.extend(arr.iter().cloned());
            }
            if call_tool {
                let tool_lines = rng.random_range(18..48);
                let tool_out = pseudo_code_diff(&mut rng, tool_lines);
                history.push(json!({
                    "role": "tool",
                    "parts": [{"type": "tool_call_response", "id": format!("call_{traj_idx}_{turn}"), "response": tool_out}]
                }));
            }
            rows.push(s);
            t += dur + 20_000_000;
        }
        rows[root_idx].end_time = t;
        rows[root_idx].duration_ns = t - rows[root_idx].start_time;
        traj_idx += 1;
    }
    rows.truncate(target_rows);
    rows
}

fn load_swe_trajectories(dir: &Path, target_rows: usize) -> BenchResult<Vec<SpanRow>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("parquet"))
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Ok(generate_w1_agent_trajectories(target_rows));
    }

    let mut rng = SmallRng::seed_from_u64(0x5731_0002);
    let mut rows = Vec::with_capacity(target_rows);
    'outer: for p in paths {
        let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(&p)?)?;
        let schema = builder.parquet_schema();
        let idx: Vec<usize> = (0..schema.num_columns())
            .filter(|&i| schema.column(i).path().parts()[0] == "trajectory")
            .collect();
        let mask = ProjectionMask::leaves(schema, idx);
        for batch in builder.with_projection(mask).with_batch_size(128).build()? {
            let batch = batch?;
            let Some(col) = batch.column_by_name("trajectory") else {
                continue;
            };
            let list = col.as_list::<i32>();
            for t_idx in 0..list.len() {
                let msgs = list.value(t_idx);
                let st: &StructArray = msgs.as_struct();
                let role = st.column_by_name("role").map(|c| c.as_string::<i32>());
                let text = st.column_by_name("text").map(|c| c.as_string::<i32>());
                let sys = st
                    .column_by_name("system_prompt")
                    .map(|c| c.as_string::<i32>());
                let (Some(role), Some(text)) = (role, text) else {
                    continue;
                };
                let tid = make_trace_id(&mut rng);
                let root_sid = make_span_id(&mut rng);
                let (provider, model) = MODELS[rng.random_range(0..MODELS.len())];
                let mut ts = DAY_START + rng.random_range(0..DAY_NS);
                let mut system = String::new();
                let mut history: Vec<serde_json::Value> = Vec::new();
                for m in 0..st.len() {
                    let r = role.value(m);
                    let txt = if text.is_null(m) { "" } else { text.value(m) };
                    if let Some(sys_col) = sys
                        && !sys_col.is_null(m)
                        && !sys_col.value(m).is_empty()
                    {
                        system = sys_col.value(m).to_owned();
                    }
                    if r == "system" {
                        system = txt.to_owned();
                        continue;
                    }
                    if r == "ai" || r == "assistant" {
                        let input_json = serde_json::to_string(&history)?;
                        let output_json = serde_json::to_string(&json!([{
                            "role": "assistant",
                            "parts": [{"type": "text", "content": txt}],
                            "finish_reason": "stop"
                        }]))?;
                        let sys_json =
                            serde_json::to_string(&json!([{"type":"text","content":system}]))?;
                        let dur = rng.random_range(200..3_000) * 1_000_000;
                        let span_id = make_span_id(&mut rng);
                        let mut s = base_span(
                            &mut rng,
                            tid,
                            span_id,
                            Some(root_sid),
                            format!("chat {model}"),
                            "llm",
                            ts,
                            ts + dur,
                        );
                        let in_tok = (input_json.len() / 4) as i64;
                        let out_tok = (output_json.len() / 4) as i64;
                        s.provider = Some(provider.to_owned());
                        s.request_model = Some(model.to_owned());
                        s.model = Some(model.to_owned());
                        s.input_tokens = Some(in_tok);
                        s.output_tokens = Some(out_tok);
                        s.total_tokens = Some(in_tok + out_tok);
                        s.cost_input = Some(in_tok as f64 * 2.5e-6);
                        s.cost_output = Some(out_tok as f64 * 1e-5);
                        s.cost_total = Some(in_tok as f64 * 2.5e-6 + out_tok as f64 * 1e-5);
                        s.input = Some(input_json);
                        s.input_format = Some("messages".to_owned());
                        s.output = Some(output_json);
                        s.output_format = Some("messages".to_owned());
                        s.input_preview = s.input.as_deref().map(preview);
                        s.output_preview = s.output.as_deref().map(preview);
                        s.system_instructions = Some(sys_json);
                        if rng.random_bool(0.03) {
                            s.status = "error".to_owned();
                        }
                        rows.push(s);
                        ts += dur + 10_000_000;
                        if rows.len() >= target_rows {
                            break 'outer;
                        }
                    }
                    let role_out = if r == "ai" { "assistant" } else { r };
                    history.push(
                        json!({"role": role_out, "parts": [{"type": "text", "content": txt}]}),
                    );
                }
            }
        }
    }
    Ok(rows)
}

fn generate_w2_chat_slices(per_slice: usize) -> Vec<Vec<SpanRow>> {
    let mut rng = SmallRng::seed_from_u64(0x5732_0001);
    let num_slices = 4usize;
    let total_sessions = (per_slice * num_slices) / 8;
    let sys_prompts: Vec<String> = (0..8)
        .map(|i| {
            format!(
                "System assistant persona #{i}: Be helpful, concise, accurate, and cite code or documentation \
                 clearly. Follow safety and privacy policies for workspace tenant {i}."
            )
            .repeat(6)
        })
        .collect();

    let mut slices: Vec<Vec<SpanRow>> = vec![Vec::with_capacity(per_slice); num_slices];
    for sess_idx in 0..total_sessions {
        let sess_name = format!("chat-sess-{sess_idx:05}");
        let user = format!("user-{}", sess_idx % 400);
        let sys = serde_json::to_string(&json!([{
            "type": "text",
            "content": sys_prompts[sess_idx % sys_prompts.len()]
        }]))
        .unwrap_or_default();
        let (provider, model) = MODELS[sess_idx % MODELS.len()];
        let mut history: Vec<serde_json::Value> = Vec::new();
        let turns_per_session = 8usize;
        for turn in 0..turns_per_session {
            let slice_idx = (turn * num_slices) / turns_per_session;
            if slices[slice_idx].len() >= per_slice {
                continue;
            }
            let slice_start = DAY_START + (slice_idx as i64) * 6 * HOUR_NS;
            let ts = slice_start + rng.random_range(0..(5 * HOUR_NS));
            let dur = rng.random_range(300..2_500) * 1_000_000;
            let user_q = format!(
                "Turn {turn} in session {sess_name}: Explain how to optimize columnar range scans \
                 with 1 MiB read caches and bloom pruning for case {}.",
                (sess_idx * 17 + turn) % 997
            );
            history.push(json!({"role": "user", "parts": [{"type": "text", "content": user_q}]}));
            let input_json = serde_json::to_string(&history).unwrap_or_default();
            let ans = format!(
                "Answer for turn {turn}: pack hot metadata into the trailing 1 MiB block, use SBBF \
                 for high-cardinality IDs, and deduplicate repeated conversation prefixes into a day pack. {}",
                pseudo_code_diff(&mut rng, 4)
            );
            let out_val = json!([{"role":"assistant","parts":[{"type":"text","content":ans}],"finish_reason":"stop"}]);
            let output_json = serde_json::to_string(&out_val).unwrap_or_default();
            let tid = make_trace_id(&mut rng);
            let sid = make_span_id(&mut rng);
            let mut s = base_span(
                &mut rng,
                tid,
                sid,
                None,
                format!("chat {model}"),
                "llm",
                ts,
                ts + dur,
            );
            let in_tok = (input_json.len() / 4) as i64;
            let out_tok = (output_json.len() / 4) as i64;
            s.session_id = Some(sess_name.clone());
            s.user_id = Some(user.clone());
            s.provider = Some(provider.to_owned());
            s.request_model = Some(model.to_owned());
            s.model = Some(model.to_owned());
            s.operation = Some("chat".to_owned());
            s.input_tokens = Some(in_tok);
            s.output_tokens = Some(out_tok);
            s.total_tokens = Some(in_tok + out_tok);
            s.cost_input = Some(in_tok as f64 * 1.5e-6);
            s.cost_output = Some(out_tok as f64 * 6.0e-6);
            s.cost_total = Some(in_tok as f64 * 1.5e-6 + out_tok as f64 * 6.0e-6);
            s.input = Some(input_json);
            s.input_format = Some("messages".to_owned());
            s.output = Some(output_json);
            s.output_format = Some("messages".to_owned());
            s.input_preview = s.input.as_deref().map(preview);
            s.output_preview = s.output.as_deref().map(preview);
            s.system_instructions = Some(sys.clone());
            if rng.random_bool(0.03) {
                s.status = "error".to_owned();
            }
            history.push(json!({"role":"assistant","parts":[{"type":"text","content":ans}]}));
            slices[slice_idx].push(s);
        }
    }
    slices
}

fn load_oasst2_slices(path: &Path, per_slice: usize) -> BenchResult<Vec<Vec<SpanRow>>> {
    if !path.exists() {
        return Ok(generate_w2_chat_slices(per_slice));
    }
    let file = File::open(path)?;
    let reader: Box<dyn BufRead> = if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        Box::new(BufReader::new(GzDecoder::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };

    #[derive(Clone)]
    struct Msg {
        parent_id: Option<String>,
        role: String,
        text: String,
    }

    let mut msgs: HashMap<String, Msg> = HashMap::new();
    let mut order = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            let Some(id) = v.get("message_id").and_then(|x| x.as_str()) else {
                continue;
            };
            let role = v
                .get("role")
                .and_then(|x| x.as_str())
                .unwrap_or("prompter")
                .to_owned();
            let text = v
                .get("text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_owned();
            let parent_id = v
                .get("parent_id")
                .and_then(|x| x.as_str())
                .map(str::to_owned);
            msgs.insert(
                id.to_owned(),
                Msg {
                    parent_id,
                    role,
                    text,
                },
            );
            order.push(id.to_owned());
            if order.len() >= per_slice * 16 {
                break;
            }
        }
    }

    let mut rng = SmallRng::seed_from_u64(0x5732_0002);
    let mut slices = vec![Vec::with_capacity(per_slice); 4];
    let mut slice_cursor = 0usize;
    for id in order {
        if slices.iter().all(|s| s.len() >= per_slice) {
            break;
        }
        let Some(m) = msgs.get(&id) else {
            continue;
        };
        if m.role != "assistant" {
            continue;
        }
        let mut chain = Vec::new();
        let mut cur = m.parent_id.clone();
        while let Some(pid) = cur {
            if let Some(pm) = msgs.get(&pid) {
                chain.push(
                    json!({"role": pm.role, "parts": [{"type": "text", "content": pm.text}]}),
                );
                cur = pm.parent_id.clone();
            } else {
                break;
            }
            if chain.len() >= 16 {
                break;
            }
        }
        chain.reverse();
        let slice_idx = slice_cursor % 4;
        slice_cursor += 1;
        if slices[slice_idx].len() >= per_slice {
            continue;
        }
        let slice_start = DAY_START + (slice_idx as i64) * 6 * HOUR_NS;
        let ts = slice_start + rng.random_range(0..(5 * HOUR_NS));
        let dur = rng.random_range(300..2_000) * 1_000_000;
        let (provider, model) = MODELS[rng.random_range(0..MODELS.len())];
        let tid = make_trace_id(&mut rng);
        let sid = make_span_id(&mut rng);
        let mut s = base_span(
            &mut rng,
            tid,
            sid,
            None,
            format!("chat {model}"),
            "llm",
            ts,
            ts + dur,
        );
        let inp = serde_json::to_string(&chain)?;
        let out = serde_json::to_string(
            &json!([{"role":"assistant","parts":[{"type":"text","content":m.text}],"finish_reason":"stop"}]),
        )?;
        s.provider = Some(provider.to_owned());
        s.model = Some(model.to_owned());
        s.input = Some(inp);
        s.output = Some(out);
        s.input_preview = s.input.as_deref().map(preview);
        s.output_preview = s.output.as_deref().map(preview);
        slices[slice_idx].push(s);
    }
    if slices.iter().any(|s| s.is_empty()) {
        return Ok(generate_w2_chat_slices(per_slice));
    }
    Ok(slices)
}

fn generate_w3_narrow_spans(count: usize) -> Vec<SpanRow> {
    let mut rng = SmallRng::seed_from_u64(0x5733_0001);
    let mut rows: Vec<SpanRow> = Vec::with_capacity(count);
    for i in 0..count {
        let tid = if i % 4 == 0 || rows.is_empty() {
            make_trace_id(&mut rng)
        } else {
            rows[rows.len() - 1].trace_id
        };
        let sid = make_span_id(&mut rng);
        let ts = DAY_START + rng.random_range(0..DAY_NS);
        let dur = rng.random_range(5_000_000..4_500_000_000);
        let (provider, model) = MODELS[rng.random_range(0..MODELS.len())];
        let kind = match i % 5 {
            0 => "agent",
            1 | 2 => "llm",
            3 => "tool",
            _ => "chain",
        };
        let mut s = base_span(
            &mut rng,
            tid,
            sid,
            None,
            if kind == "llm" {
                format!("chat {model}")
            } else {
                format!("{kind}.exec")
            },
            kind,
            ts,
            ts + dur,
        );
        if kind == "llm" {
            let in_tok = rng.random_range(80..2_400);
            let out_tok = rng.random_range(20..800);
            s.provider = Some(provider.to_owned());
            s.request_model = Some(model.to_owned());
            s.response_model = Some(model.to_owned());
            s.model = Some(model.to_owned());
            s.operation = Some("chat".to_owned());
            s.input_tokens = Some(in_tok);
            s.output_tokens = Some(out_tok);
            s.total_tokens = Some(in_tok + out_tok);
            s.cost_input = Some(in_tok as f64 * 2.0e-6);
            s.cost_output = Some(out_tok as f64 * 8.0e-6);
            s.cost_total = Some(in_tok as f64 * 2.0e-6 + out_tok as f64 * 8.0e-6);
            s.cost_source = Some("computed".to_owned());
            s.ttft_ns = Some(dur / 5);
        }
        if rng.random_bool(0.03) {
            s.status = "error".to_owned();
            s.status_message = Some("RateLimitError: 429".to_owned());
        }
        s.attributes
            .insert("http.status_code".to_owned(), Some("200".to_owned()));
        if i % 10 == 0 {
            s.events.push(SpanEventRow {
                time: ts + dur / 2,
                name: "token.first".to_owned(),
                attributes: Some("{\"ms\":120}".to_owned()),
            });
        }
        if i % 25 == 0 {
            s.links.push(SpanLinkRow {
                trace_id: tid,
                span_id: sid,
                attributes: None,
            });
        }
        rows.push(s);
    }
    rows
}

fn generate_w3_scores(count: usize) -> Vec<ScoreRow> {
    let mut rng = SmallRng::seed_from_u64(0x5733_0002);
    let score_names = [
        "faithfulness",
        "relevance",
        "toxicity",
        "helpfulness",
        "latency_ok",
    ];
    let mut rows = Vec::with_capacity(count);
    for i in 0..count {
        let ts = DAY_START + rng.random_range(0..DAY_NS);
        let name = score_names[i % score_names.len()];
        let is_num = i % 3 != 2;
        let mut md = BTreeMap::new();
        md.insert(
            "customer_id".to_owned(),
            Some(if i % 50 == 0 {
                "cust-42".to_owned()
            } else {
                format!("cust-{}", rng.random_range(0..500))
            }),
        );
        rows.push(ScoreRow {
            project_id: PROJECT_ID.to_owned(),
            score_id: format!("018f6b4e-7c1a-7c1a-9000-{i:012x}"),
            target: if i % 4 == 0 { "span" } else { "trace" }.to_owned(),
            trace_id: Some(make_trace_id(&mut rng)),
            span_id: (i % 4 == 0).then(|| make_span_id(&mut rng)),
            session_id: Some(format!("sess-{:06}", rng.random_range(0..5_000))),
            user_id: Some(format!("user-{}", rng.random_range(0..2_000))),
            name: name.to_owned(),
            data_type: if is_num { "numeric" } else { "categorical" }.to_owned(),
            value_num: is_num.then(|| (rng.random_range(0..101) as f64) / 100.0),
            value_str: (!is_num).then(|| if i % 2 == 0 { "pass" } else { "fail" }.to_owned()),
            reason: (i % 5 == 0).then(|| "automated judge evaluation".to_owned()),
            source: if i % 3 == 0 { "api" } else { "eval" }.to_owned(),
            evaluator_id: Some(format!("eval-{}", i % 8)),
            rule_id: None,
            experiment_id: (i % 10 == 0).then(|| format!("exp-{}", i % 4)),
            dataset_item_id: None,
            queue_id: None,
            author_user_id: None,
            metadata: md,
            created_at: ts,
            updated_at: ts + NS,
            deleted: false,
            ingest_time: ts + 2 * NS,
        });
    }
    rows
}

fn random_high_entropy_string(rng: &mut SmallRng, len_chars: usize) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(len_chars);
    let mut acc = 0u64;
    let mut bits = 0u32;
    for _ in 0..len_chars {
        if bits < 6 {
            acc = rng.random::<u64>();
            bits = 64;
        }
        s.push(B64[(acc & 0x3F) as usize] as char);
        acc >>= 6;
        bits -= 6;
    }
    s
}

fn generate_w4_adversarial(count: usize) -> Vec<SpanRow> {
    let mut rng = SmallRng::seed_from_u64(0x5734_0001);
    let mut rows = Vec::with_capacity(count);
    for i in 0..count {
        let tid = make_trace_id(&mut rng);
        let sid = make_span_id(&mut rng);
        let ts = DAY_START + rng.random_range(0..DAY_NS);
        let dur = rng.random_range(100..5_000_000_000);
        let op_name = format!("adv-op-{}", random_high_entropy_string(&mut rng, 16));
        let mut s = base_span(&mut rng, tid, sid, None, op_name, "llm", ts, ts + dur);
        // High-cardinality unique UUIDs (defeats dictionary compression).
        s.session_id = Some(format!("sess-{}", random_high_entropy_string(&mut rng, 24)));
        s.user_id = Some(format!("user-{}", random_high_entropy_string(&mut rng, 24)));
        s.model = Some(MODELS[i % MODELS.len()].1.to_owned());
        s.provider = Some(MODELS[i % MODELS.len()].0.to_owned());

        // Adversarial floats: NaN, +/-inf, -0.0, mixed nulls.
        s.cost_input = match i % 7 {
            0 => Some(f64::NAN),
            1 => Some(f64::INFINITY),
            2 => Some(f64::NEG_INFINITY),
            3 => Some(-0.0),
            4 => None,
            _ => Some(rng.random_range(1..10_000) as f64 * 1e-5),
        };
        s.cost_total = match i % 6 {
            0 => Some(f64::NAN),
            1 => Some(-0.0),
            2 => None,
            _ => Some(rng.random_range(1..10_000) as f64 * 1e-5),
        };
        s.total_tokens = (i % 4 != 0).then(|| rng.random_range(10..50_000));

        // High-entropy incompressible payloads (~2 KiB each, or None on mixed rows).
        if i % 5 != 0 {
            s.input = Some(random_high_entropy_string(&mut rng, 2048));
            s.output = Some(random_high_entropy_string(&mut rng, 1024));
        }

        // 5,000 distinct dynamic keys across `attributes`!
        s.attributes.clear();
        if i % 8 != 0 {
            for k in 0..5 {
                let key_id = (i * 5 + k) % 5_000;
                s.attributes.insert(
                    format!("attr_k_{key_id:04}"),
                    if k == 4 && i % 3 == 0 {
                        None
                    } else {
                        Some(random_high_entropy_string(&mut rng, 16))
                    },
                );
            }
        } else {
            // Empty collections on 1/8th of rows.
            s.tags.clear();
            s.metadata.clear();
            s.resource_attributes.clear();
            s.events.clear();
            s.links.clear();
        }
        if i % 11 == 0 {
            s.status = "error".to_owned();
        }
        rows.push(s);
    }
    rows
}

fn generate_w4_tiny_flushes(num_files: usize) -> Vec<Vec<SpanRow>> {
    let mut rng = SmallRng::seed_from_u64(0x5734_0099);
    let mut files = Vec::with_capacity(num_files);
    for i in 0..num_files {
        let tid = make_trace_id(&mut rng);
        let sid = make_span_id(&mut rng);
        let ts = DAY_START + (i as i64) * 60 * NS;
        let mut s = base_span(
            &mut rng,
            tid,
            sid,
            None,
            "tiny.flush.span".to_owned(),
            "llm",
            ts,
            ts + 500_000_000,
        );
        s.model = Some("gpt-4o".to_owned());
        s.total_tokens = Some(128);
        s.cost_total = Some(0.0012);
        s.input = Some(random_high_entropy_string(&mut rng, 512));
        s.output = Some(random_high_entropy_string(&mut rng, 256));
        files.push(vec![s]);
    }
    files
}
