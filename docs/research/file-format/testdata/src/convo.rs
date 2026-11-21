//! Conversations written by `bench/convert_convos.py` (Open-SWE-Traces,
//! Toucan-1.5M, WildChat-1M) turned into spans: one trace per conversation,
//! one LLM span per assistant message and one tool span per tool message.
//!
//! Input layout, one row per conversation: `system` (plain text, nullable),
//! `tools` (JSON array, nullable), `model` (nullable) and `messages`
//! (`list<string>`, each an OTel GenAI message as JSON).

use std::path::PathBuf;

use arrow::array::{Array, AsArray};
use cetra_core::span::Span;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_json::Value;

use crate::datagen;
use crate::swe::{Step, trace_spans};

type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Spans built from at most `max_conversations` conversations, read from the
/// given files in order.
pub fn spans(
    paths: &[PathBuf],
    max_conversations: usize,
    world: &datagen::World,
    rng: &mut datagen::Rng,
) -> R<Vec<Span>> {
    let mut out = Vec::new();
    let mut index = 0usize;
    'outer: for f in paths {
        let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(f)?)?
            .with_batch_size(256)
            .build()?;
        for batch in reader {
            let batch = batch?;
            let col = |name: &str| {
                batch
                    .column_by_name(name)
                    .ok_or_else(|| format!("convo parquet: missing column {name}"))
            };
            let system = col("system")?.as_string::<i32>();
            let tools = col("tools")?.as_string::<i32>();
            let messages = col("messages")?.as_list::<i32>();
            for row in 0..batch.num_rows() {
                if index == max_conversations {
                    break 'outer;
                }
                let sys = (!system.is_null(row)).then(|| system.value(row));
                let tl = (!tools.is_null(row)).then(|| tools.value(row));
                let msgs = messages.value(row);
                let msgs: Vec<&str> = (0..msgs.len())
                    .map(|i| msgs.as_string::<i32>().value(i))
                    .collect();
                let (first_user, last_ai, steps) = conversation_steps(sys, tl, &msgs)?;
                trace_spans(index, world, rng, first_user, last_ai, steps, &mut out);
                index += 1;
            }
        }
    }
    Ok(out)
}

/// The root's question and answer, and the steps after the root, for one
/// conversation. The input of the LLM call at assistant message `k` is the
/// JSON array of messages `0..k`; its output is message `k`.
fn conversation_steps(
    system: Option<&str>,
    tools: Option<&str>,
    msgs: &[&str],
) -> R<(Option<String>, Option<String>, Vec<Step>)> {
    let system_instructions =
        system.map(|s| serde_json::json!([{"type":"text","content":s}]).to_string());
    let mut first_user = None;
    let mut last_ai = None;
    let mut steps = Vec::new();
    // Arguments of each tool call so far, by call id, for the tool spans' input.
    let mut calls: Vec<(Option<String>, String)> = Vec::new();
    let mut history = String::from("[");
    for m in msgs {
        let v: Value = serde_json::from_str(m)?;
        let parts = v["parts"].as_array().map_or(&[][..], Vec::as_slice);
        match v["role"].as_str() {
            Some("user") => {
                if first_user.is_none() {
                    first_user = Some(texts(parts));
                }
            }
            Some("assistant") => {
                let text = texts(parts);
                for p in parts.iter().filter(|p| p["type"] == "tool_call") {
                    calls.push((
                        p["id"].as_str().map(str::to_owned),
                        p["arguments"].to_string(),
                    ));
                }
                let preview = if text.is_empty() {
                    parts
                        .iter()
                        .find_map(|p| p["name"].as_str())
                        .unwrap_or_default()
                        .to_owned()
                } else {
                    text.clone()
                };
                steps.push(Step::Llm {
                    input: format!("{history}]"),
                    output: format!("[{m}]"),
                    system_instructions: system_instructions.clone(),
                    tool_definitions: tools.map(str::to_owned),
                    text: preview,
                });
                if !text.is_empty() {
                    last_ai = Some(text);
                }
            }
            Some("tool") => {
                for p in parts {
                    let id = p["id"].as_str();
                    let input = calls
                        .iter()
                        .rev()
                        .find(|(c, _)| c.as_deref() == id)
                        .or(calls.last())
                        .map(|(_, a)| a.clone());
                    let text = match &p["response"] {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    steps.push(Step::Tool { input, text });
                }
            }
            _ => return Err("convo parquet: a message role is not user, assistant or tool".into()),
        }
        if history.len() > 1 {
            history.push(',');
        }
        history.push_str(m);
    }
    Ok((first_user, last_ai, steps))
}

/// The text parts of a message, joined by blank lines.
fn texts(parts: &[Value]) -> String {
    let t: Vec<&str> = parts
        .iter()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["content"].as_str())
        .collect();
    t.join("\n\n")
}

#[cfg(test)]
mod tests {
    use cetra_core::batch::spans_to_record_batch;
    use cetra_core::enums::{PayloadFormat, SpanKind};

    use super::*;

    const AGENT: [&str; 5] = [
        r#"{"role":"user","parts":[{"type":"text","content":"Fix the parser."}]}"#,
        r#"{"role":"assistant","parts":[{"type":"reasoning","content":"look first"},{"type":"tool_call","id":"c1","name":"bash","arguments":{"cmd":"ls"}}],"finish_reason":"tool_calls"}"#,
        r#"{"role":"tool","parts":[{"type":"tool_call_response","id":"c1","response":"parser.rs"}]}"#,
        r#"{"role":"assistant","parts":[{"type":"text","content":"Patched."}],"finish_reason":"stop"}"#,
        r#"{"role":"user","parts":[{"type":"text","content":"Thanks"}]}"#,
    ];

    #[test]
    fn an_agent_conversation_gives_llm_and_tool_steps_in_order() {
        let (q, a, steps) =
            conversation_steps(Some("Be careful."), Some("[{\"name\":\"bash\"}]"), &AGENT).unwrap();
        assert_eq!(q.as_deref(), Some("Fix the parser."));
        assert_eq!(a.as_deref(), Some("Patched."));
        assert_eq!(steps.len(), 3);
        let Step::Llm {
            input,
            output,
            system_instructions,
            tool_definitions,
            text,
        } = &steps[0]
        else {
            panic!("first step is the LLM call")
        };
        assert_eq!(input, &format!("[{}]", AGENT[0]));
        assert_eq!(output, &format!("[{}]", AGENT[1]));
        let si: Value = serde_json::from_str(system_instructions.as_deref().unwrap()).unwrap();
        assert_eq!(
            si,
            serde_json::json!([{"type":"text","content":"Be careful."}])
        );
        assert_eq!(tool_definitions.as_deref(), Some("[{\"name\":\"bash\"}]"));
        assert_eq!(text, "bash");
        let Step::Tool { input, text } = &steps[1] else {
            panic!("second step is the tool result")
        };
        assert_eq!(input.as_deref(), Some(r#"{"cmd":"ls"}"#));
        assert_eq!(text, "parser.rs");
        let Step::Llm { input, .. } = &steps[2] else {
            panic!("third step is the LLM call")
        };
        assert_eq!(input, &format!("[{},{},{}]", AGENT[0], AGENT[1], AGENT[2]));
        // The input is valid JSON.
        serde_json::from_str::<Value>(input).unwrap();
    }

    #[test]
    fn a_chat_gives_only_llm_steps_without_system_or_tools() {
        let chat = [
            r#"{"role":"user","parts":[{"type":"text","content":"Hi"}]}"#,
            r#"{"role":"assistant","parts":[{"type":"text","content":"Hello"}],"finish_reason":"stop"}"#,
            r#"{"role":"user","parts":[{"type":"text","content":"Bye"}]}"#,
            r#"{"role":"assistant","parts":[{"type":"text","content":"Goodbye"}],"finish_reason":"stop"}"#,
        ];
        let (q, a, steps) = conversation_steps(None, None, &chat).unwrap();
        assert_eq!((q.as_deref(), a.as_deref()), (Some("Hi"), Some("Goodbye")));
        assert_eq!(steps.len(), 2);
        assert!(steps.iter().all(|s| matches!(
            s,
            Step::Llm {
                system_instructions: None,
                tool_definitions: None,
                ..
            }
        )));
    }

    #[test]
    fn an_unknown_role_is_an_error() {
        assert!(conversation_steps(None, None, &[r#"{"role":"function","parts":[]}"#]).is_err());
    }

    #[test]
    fn a_file_builds_valid_spans() {
        use std::sync::Arc;

        use arrow::array::{ArrayRef, ListBuilder, RecordBatch, StringArray, StringBuilder};
        use arrow::datatypes::{DataType, Field, Schema};
        use parquet::arrow::ArrowWriter;

        let mut lb = ListBuilder::new(StringBuilder::new());
        for m in AGENT {
            lb.values().append_value(m);
        }
        lb.append(true);
        let messages = lb.finish();
        let schema = Arc::new(Schema::new(vec![
            Field::new("system", DataType::Utf8, true),
            Field::new("tools", DataType::Utf8, true),
            Field::new("model", DataType::Utf8, true),
            Field::new("messages", messages.data_type().clone(), true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec![Some("sys")])) as ArrayRef,
                Arc::new(StringArray::from(vec![None::<&str>])) as ArrayRef,
                Arc::new(StringArray::from(vec![Some("m")])) as ArrayRef,
                Arc::new(messages) as ArrayRef,
            ],
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("convo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.parquet");
        let mut w =
            ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();

        let world = datagen::World::new(7);
        let mut rng = datagen::Rng::new(42);
        let result = spans(std::slice::from_ref(&path), 10, &world, &mut rng);
        let _ = std::fs::remove_dir_all(&dir);
        let out = result.unwrap();

        let kinds: Vec<SpanKind> = out.iter().map(|s| s.span_kind).collect();
        assert_eq!(
            kinds,
            vec![
                SpanKind::Agent,
                SpanKind::Llm,
                SpanKind::Tool,
                SpanKind::Llm
            ]
        );
        assert_eq!(out[2].input_format, Some(PayloadFormat::Json));
        assert_eq!(out[2].input.as_deref(), Some(r#"{"cmd":"ls"}"#));
        spans_to_record_batch(&out).unwrap();
    }
}
