//! SWE-agent trajectories (`nebius/SWE-agent-trajectories`) turned into LLM
//! payload values: one `input`, `output` and `system_instructions` value per
//! assistant message, grouped into column chunks of roughly `budget_bytes`.

use std::path::PathBuf;

use arrow::array::{Array, AsArray, StructArray};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

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
