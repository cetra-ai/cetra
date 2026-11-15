//! SWE-agent trajectories (`nebius/SWE-agent-trajectories`) turned into LLM
//! payload values: one `input`, `output` and `system_instructions` value per
//! assistant message, grouped into column chunks of roughly `budget_bytes`.

use std::path::PathBuf;

use arrow::array::{Array, AsArray, StructArray};
use llmobs_core::enums::{CostSource, PayloadFormat, SpanKind, SpanStatus};
use llmobs_core::ids::{SpanId, TraceId};
use llmobs_core::span::Span;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use xxhash_rust::xxh3::xxh3_128;

use crate::datagen;

/// Result of [`payloads`]. `chunks[k]` holds the values of chunk `k` as
/// `[input, output, system_instructions]`.
pub struct Payloads {
    pub chunks: Vec<[Vec<Vec<u8>>; 3]>,
    pub trajectories: usize,
    pub spans: usize,
    /// A substring of a mid-trajectory reply (or `zqxjneedle` when none qualifies).
    pub needle: Vec<u8>,
}

/// Reads the `trajectory` column of the given Parquet files and builds payload
/// chunks. Stops after `max_chunks` chunks.
pub fn payloads(
    paths: &[PathBuf],
    budget_bytes: usize,
    max_chunks: usize,
) -> Result<Payloads, Box<dyn std::error::Error + Send + Sync>> {
    let mut chunks: Vec<[Vec<Vec<u8>>; 3]> = Vec::new();
    let new_chunk = || [Vec::new(), Vec::new(), Vec::new()];
    let mut cur = new_chunk();
    let mut cur_bytes = 0usize;
    let mut trajectories = 0usize;
    let mut spans = 0usize;
    let mut needle: Option<Vec<u8>> = None;
    'outer: for f in paths {
        let b = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(f)?)?;
        let schema = b.parquet_schema();
        let idx: Vec<usize> = (0..schema.num_columns()).filter(|&i| schema.column(i).path().parts()[0] == "trajectory").collect();
        let mask = ProjectionMask::leaves(schema, idx);
        for batch in b.with_projection(mask).with_batch_size(256).build()? {
            let batch = batch?;
            let list = batch.column_by_name("trajectory").ok_or("swe parquet: missing column trajectory")?.as_list::<i32>();
            for t in 0..list.len() {
                let msgs = list.value(t);
                let st: &StructArray = msgs.as_struct();
                let role = st.column_by_name("role").ok_or("swe parquet: missing column role")?.as_string::<i32>();
                let text = st.column_by_name("text").ok_or("swe parquet: missing column text")?.as_string::<i32>();
                let sys = st.column_by_name("system_prompt").ok_or("swe parquet: missing column system_prompt")?.as_string::<i32>();
                let mut system = String::new();
                let mut history: Vec<serde_json::Value> = Vec::new();
                trajectories += 1;
                for m in 0..st.len() {
                    let r = role.value(m);
                    let txt = if text.is_null(m) { "" } else { text.value(m) };
                    if !sys.is_null(m) && !sys.value(m).is_empty() {
                        system = sys.value(m).to_owned();
                    }
                    if r == "system" {
                        system = txt.to_owned();
                        continue;
                    }
                    if r == "ai" || r == "assistant" {
                        let input = serde_json::to_string(&history)?;
                        let out = serde_json::to_string(&serde_json::json!([{"role":"assistant","parts":[{"type":"text","content":txt}],"finish_reason":"stop"}]))?;
                        let sysj = serde_json::to_string(&serde_json::json!([{"type":"text","content":system}]))?;
                        if needle.is_none() && trajectories == 37 && txt.len() > 400 {
                            let mid = &txt.as_bytes()[txt.len() / 2..];
                            if let Some(p) = mid.iter().position(|c| *c == b' ') {
                                let n = &mid[p + 1..(p + 1 + 20).min(mid.len())];
                                needle = Some(n.to_vec());
                            }
                        }
                        cur_bytes += input.len();
                        cur[0].push(input.into_bytes());
                        cur[1].push(out.into_bytes());
                        cur[2].push(sysj.into_bytes());
                        spans += 1;
                    }
                    let role_out = if r == "ai" { "assistant" } else { r };
                    history.push(serde_json::json!({"role":role_out,"parts":[{"type":"text","content":txt}]}));
                }
                if cur_bytes >= budget_bytes {
                    chunks.push(std::mem::replace(&mut cur, new_chunk()));
                    cur_bytes = 0;
                    if chunks.len() == max_chunks {
                        break 'outer;
                    }
                }
            }
        }
    }
    if chunks.len() < max_chunks && !cur[0].is_empty() {
        chunks.push(cur);
    }
    let needle = needle.unwrap_or_else(|| b"zqxjneedle".to_vec());
    Ok(Payloads { chunks, trajectories, spans, needle })
}

/// Spans built from SWE-agent trajectories: one trace per trajectory, at most
/// `max_trajectories`, read from the given shards in order.
pub fn spans(
    paths: &[PathBuf],
    max_trajectories: usize,
    world: &datagen::World,
    rng: &mut datagen::Rng,
) -> Result<Vec<Span>, Box<dyn std::error::Error + Send + Sync>> {
    let mut out = Vec::new();
    let mut index = 0usize;
    'outer: for f in paths {
        let b = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(f)?)?;
        let schema = b.parquet_schema();
        let idx: Vec<usize> = (0..schema.num_columns()).filter(|&i| schema.column(i).path().parts()[0] == "trajectory").collect();
        let mask = ProjectionMask::leaves(schema, idx);
        for batch in b.with_projection(mask).with_batch_size(256).build()? {
            let batch = batch?;
            let list = batch.column_by_name("trajectory").ok_or("swe parquet: missing column trajectory")?.as_list::<i32>();
            for t in 0..list.len() {
                if index == max_trajectories {
                    break 'outer;
                }
                let msgs = list.value(t);
                let st: &StructArray = msgs.as_struct();
                let role = st.column_by_name("role").ok_or("swe parquet: missing column role")?.as_string::<i32>();
                let text = st.column_by_name("text").ok_or("swe parquet: missing column text")?.as_string::<i32>();
                let sys = st.column_by_name("system_prompt").ok_or("swe parquet: missing column system_prompt")?.as_string::<i32>();
                let mut system = String::new();
                let mut history: Vec<serde_json::Value> = Vec::new();
                let mut first_user: Option<String> = None;
                let mut last_ai: Option<String> = None;
                let mut steps: Vec<Step> = Vec::new();
                for m in 0..st.len() {
                    let r = role.value(m);
                    let txt = if text.is_null(m) { "" } else { text.value(m) };
                    if !sys.is_null(m) && !sys.value(m).is_empty() {
                        system = sys.value(m).to_owned();
                    }
                    if r == "system" {
                        system = txt.to_owned();
                        continue;
                    }
                    if r == "ai" || r == "assistant" {
                        steps.push(Step::Llm {
                            input: serde_json::to_string(&history)?,
                            output: serde_json::to_string(&serde_json::json!([{"role":"assistant","parts":[{"type":"text","content":txt}],"finish_reason":"stop"}]))?,
                            system_instructions: serde_json::to_string(&serde_json::json!([{"type":"text","content":system}]))?,
                            text: txt.to_owned(),
                        });
                        last_ai = Some(txt.to_owned());
                    } else if r == "user" {
                        if first_user.is_none() {
                            first_user = Some(txt.to_owned());
                        } else {
                            steps.push(Step::Tool { text: txt.to_owned() });
                        }
                    }
                    let role_out = if r == "ai" { "assistant" } else { r };
                    history.push(serde_json::json!({"role":role_out,"parts":[{"type":"text","content":txt}]}));
                }
                trace_spans(index, world, rng, first_user, last_ai, steps, &mut out);
                index += 1;
            }
        }
    }
    Ok(out)
}

/// One step of a trajectory, after the root. Steps are numbered in message order.
enum Step {
    /// An `ai`/`assistant` message: the LLM call that produced `text`.
    Llm { input: String, output: String, system_instructions: String, text: String },
    /// A `user` message after the first one: the tool result `text`.
    Tool { text: String },
}

/// Appends one trace: the root, then one span per step, in order.
fn trace_spans(
    index: usize,
    world: &datagen::World,
    rng: &mut datagen::Rng,
    first_user: Option<String>,
    last_ai: Option<String>,
    steps: Vec<Step>,
    out: &mut Vec<Span>,
) {
    let mut tmp = Vec::new();
    world.trace(rng, &mut tmp);
    // `World::trace` always makes the root first and at least one LLM span.
    let root_t = fresh(tmp[0].clone());
    let llm_t = fresh(
        tmp.iter()
            .find(|s| matches!(s.span_kind, SpanKind::Llm))
            .expect("World::trace always makes an LLM span")
            .clone(),
    );
    let tool_t = {
        let mut t = llm_t.clone();
        t.span_kind = SpanKind::Tool;
        t.name = "tool".to_owned();
        t.attributes.clear();
        t.provider = None;
        t.request_model = None;
        t.response_model = None;
        t.operation = None;
        t.input_tokens = None;
        t.output_tokens = None;
        t.cache_read_tokens = None;
        t.cache_write_tokens = None;
        t.reasoning_tokens = None;
        t.total_tokens = None;
        t.cost_input = None;
        t.cost_output = None;
        t.cost_total = None;
        t.cost_source = None;
        t.ttft_ns = None;
        t.request_params = None;
        t.input = None;
        t.input_format = None;
        t.output = None;
        t.output_format = None;
        t.input_preview = None;
        t.output_preview = None;
        t.system_instructions = None;
        t.tool_definitions = None;
        fresh(t)
    };

    let trace_id = TraceId::from_bytes(xxh3_128(&(index as u64).to_le_bytes()).to_be_bytes()).expect("non-zero id");
    let span_id = |j: u64| SpanId::from_bytes((((index as u64) + 1) << 20 | j).to_be_bytes()).expect("non-zero id");
    let start = root_t.start_time;
    let step_start = |k: usize| start + (k as i64 + 1) * 2 * datagen::NS;
    let root_id = span_id(0);
    let root_end = match steps.len() {
        0 => start + datagen::NS,
        n => step_start(n - 1) + datagen::NS,
    };

    let mut root = root_t;
    root.trace_id = trace_id;
    root.span_id = root_id;
    root.parent_span_id = None;
    root.end_time = root_end;
    match &first_user {
        Some(q) => {
            root.input = Some(serde_json::json!([{"role":"user","parts":[{"type":"text","content":q}]}]).to_string());
            root.input_format = Some(PayloadFormat::Messages);
            root.input_preview = Some(datagen::preview(q));
        }
        None => {
            root.input = None;
            root.input_format = None;
            root.input_preview = None;
        }
    }
    match &last_ai {
        Some(a) => {
            root.output = Some(serde_json::json!([{"role":"assistant","parts":[{"type":"text","content":a}],"finish_reason":"stop"}]).to_string());
            root.output_format = Some(PayloadFormat::Messages);
            root.output_preview = Some(datagen::preview(a));
        }
        None => {
            root.output = None;
            root.output_format = None;
            root.output_preview = None;
        }
    }
    out.push(root);

    for (k, step) in steps.into_iter().enumerate() {
        let j = k as u64 + 1;
        let s0 = step_start(k);
        let e0 = s0 + datagen::NS;
        match step {
            Step::Llm { input, output, system_instructions, text } => {
                let in_tokens = ((input.len() + system_instructions.len()) / 4) as i64;
                let out_tokens = (output.len() / 4) as i64;
                let cost_input = in_tokens as f64 * 2.5e-6;
                let cost_output = out_tokens as f64 * 1e-5;
                let mut s = llm_t.clone();
                s.trace_id = trace_id;
                s.span_id = span_id(j);
                s.parent_span_id = Some(root_id);
                s.start_time = s0;
                s.end_time = e0;
                s.input_tokens = Some(in_tokens);
                s.output_tokens = Some(out_tokens);
                s.total_tokens = Some(in_tokens + out_tokens);
                s.cost_input = Some(cost_input);
                s.cost_output = Some(cost_output);
                s.cost_total = Some(cost_input + cost_output);
                s.cost_source = Some(CostSource::Computed);
                s.ttft_ns = Some(datagen::NS / 5);
                s.input = Some(input);
                s.input_format = Some(PayloadFormat::Messages);
                s.input_preview = first_user.as_deref().map(datagen::preview);
                s.output = Some(output);
                s.output_format = Some(PayloadFormat::Messages);
                s.output_preview = Some(datagen::preview(&text));
                s.system_instructions = Some(system_instructions);
                s.tool_definitions = None;
                out.push(s);
            }
            Step::Tool { text } => {
                let mut s = tool_t.clone();
                s.trace_id = trace_id;
                s.span_id = span_id(j);
                s.parent_span_id = Some(root_id);
                s.start_time = s0;
                s.end_time = e0;
                s.output_preview = Some(datagen::preview(&text));
                s.output = Some(text);
                s.output_format = Some(PayloadFormat::Text);
                out.push(s);
            }
        }
    }
}

/// Clears what a clone must not carry over from its template.
fn fresh(mut s: Span) -> Span {
    s.status = SpanStatus::Unset;
    s.status_message = None;
    s.events.clear();
    s.cache_read_tokens = None;
    s.cache_write_tokens = None;
    s.reasoning_tokens = None;
    s
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use arrow::array::{ArrayRef, BooleanArray, ListArray, RecordBatch, StringArray, StructArray};
    use arrow::buffer::OffsetBuffer;
    use arrow::datatypes::{DataType, Field, Fields, Schema};
    use llmobs_core::batch::spans_to_record_batch;
    use parquet::arrow::ArrowWriter;

    use super::*;

    /// Writes one `trajectory` column with the given trajectories, each a list of (role, text).
    fn write_shard(path: &std::path::Path, trajs: Vec<Vec<(&str, &str)>>) {
        let (mut cutoff, mut mask, mut role, mut sys, mut text) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut offsets = vec![0i32];
        for t in &trajs {
            for (r, x) in t {
                cutoff.push(Some("2025-01-01"));
                mask.push(Some(false));
                role.push(Some(*r));
                sys.push((*r == "system").then_some(*x));
                text.push(Some(*x));
            }
            offsets.push(role.len() as i32);
        }
        let fields = Fields::from(vec![
            Field::new("cutoff_date", DataType::Utf8, true),
            Field::new("mask", DataType::Boolean, true),
            Field::new("role", DataType::Utf8, true),
            Field::new("system_prompt", DataType::Utf8, true),
            Field::new("text", DataType::Utf8, true),
        ]);
        let values = StructArray::new(
            fields.clone(),
            vec![
                Arc::new(StringArray::from(cutoff)) as ArrayRef,
                Arc::new(BooleanArray::from(mask)) as ArrayRef,
                Arc::new(StringArray::from(role)) as ArrayRef,
                Arc::new(StringArray::from(sys)) as ArrayRef,
                Arc::new(StringArray::from(text)) as ArrayRef,
            ],
            None,
        );
        let item = Arc::new(Field::new("item", DataType::Struct(fields), true));
        let list = ListArray::new(item.clone(), OffsetBuffer::new(offsets.into()), Arc::new(values), None);
        let schema = Arc::new(Schema::new(vec![Field::new("trajectory", DataType::List(item), true)]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(list) as ArrayRef]).unwrap();
        let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }

    #[test]
    fn tiny_shard_builds_valid_spans() {
        let dir = std::env::temp_dir().join(format!("swe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shard.parquet");
        write_shard(
            &path,
            vec![
                vec![
                    ("system", "You fix bugs."),
                    ("user", "Fix the parser."),
                    ("ai", "Looking at the files."),
                    ("user", "ls output: parser.rs"),
                    ("ai", "Patched the parser."),
                    ("user", "tests pass"),
                ],
                vec![("system", "Be brief."), ("user", "Hello")],
            ],
        );
        let world = datagen::World::new(7);
        let mut rng = datagen::Rng::new(42);
        let result = spans(std::slice::from_ref(&path), 10, &world, &mut rng);
        let _ = std::fs::remove_dir_all(&dir);
        let out = result.unwrap();

        // Each trace starts with its root (5 spans, then 1); every other span points at that root.
        let roots: Vec<usize> = (0..out.len()).filter(|&i| out[i].parent_span_id.is_none()).collect();
        assert_eq!(roots, vec![0, 5]);
        let mut current_root = 0;
        for (i, s) in out.iter().enumerate() {
            if roots.contains(&i) {
                current_root = i;
                assert!(matches!(s.span_kind, SpanKind::Agent));
            } else {
                let root = &out[current_root];
                assert_eq!(s.parent_span_id, Some(root.span_id));
                assert_eq!(s.trace_id, root.trace_id);
            }
        }

        // One LLM span per ai message, one tool span per user message after the first.
        let llm = out.iter().filter(|s| matches!(s.span_kind, SpanKind::Llm)).count();
        let tool = out.iter().filter(|s| matches!(s.span_kind, SpanKind::Tool)).count();
        assert_eq!(llm, 2);
        assert_eq!(tool, (3 - 1) + (1 - 1));

        // All ids are distinct.
        let traces: HashSet<TraceId> = out.iter().map(|s| s.trace_id).collect();
        assert_eq!(traces.len(), 2);
        let spans_ids: HashSet<SpanId> = out.iter().map(|s| s.span_id).collect();
        assert_eq!(spans_ids.len(), out.len());

        spans_to_record_batch(&out).unwrap();
        for s in &out {
            assert_eq!(s.input.is_some(), s.input_format.is_some());
            assert_eq!(s.output.is_some(), s.output_format.is_some());
            if matches!(s.span_kind, SpanKind::Tool) {
                assert_eq!(s.output_format, Some(PayloadFormat::Text));
            }
        }
    }

    /// Needs the SWE shards: `FMT_SWE_SHARDS=a.parquet,b.parquet cargo test -- --ignored`.
    #[test]
    #[ignore = "needs the SWE shards in FMT_SWE_SHARDS"]
    fn real_shards_match_payloads() {
        let Ok(list) = std::env::var("FMT_SWE_SHARDS") else {
            return;
        };
        let paths: Vec<PathBuf> = list.split(',').map(PathBuf::from).collect();
        let p = payloads(&paths, 512 << 20, 4).unwrap();
        assert_eq!(p.trajectories, 1674);
        assert_eq!(p.spans, 47_198);

        let world = datagen::World::new(7);
        let mut rng = datagen::Rng::new(42);
        let out = spans(&paths, p.trajectories, &world, &mut rng).unwrap();
        let llm: Vec<&Span> = out.iter().filter(|s| matches!(s.span_kind, SpanKind::Llm)).collect();
        assert_eq!(llm.len(), p.spans);

        let len = |f: fn(&Span) -> Option<&str>| -> usize { llm.iter().map(|s| f(s).map_or(0, str::len)).sum() };
        let chunk_len = |k: usize| -> usize { p.chunks.iter().map(|c| c[k].iter().map(Vec::len).sum::<usize>()).sum() };
        assert_eq!(len(|s| s.input.as_deref()), chunk_len(0));
        assert_eq!(len(|s| s.output.as_deref()), chunk_len(1));
        assert_eq!(len(|s| s.system_instructions.as_deref()), chunk_len(2));
    }
}
