//! Provider message converters.
//!
//! Spec 01 § Canonical message format. OpenAI Chat Completions is N10.
//! Anthropic content blocks are N11. Gemini contents are N12.
//! OpenInference flattened attributes are N18. Vercel `CoreMessage` is N19.
//! OpenLLMetry flattened prompts are N20. Bedrock Converse content blocks are
//! Strands' legacy-event shape (D-IN-23).

mod anthropic;
mod converse;
mod gemini;
mod openai;
mod openinference;
mod openllmetry;
mod vercel;

// Later convention phases call the converters.
#[allow(unused_imports)]
pub(crate) use anthropic::anthropic_messages;
pub(crate) use converse::{converse_message, converse_text};
#[allow(unused_imports)]
pub(crate) use gemini::{gemini_contents, gemini_system_instruction};
pub(crate) use openai::{ConvertError, openai_messages};
pub(crate) use openinference::{flattened_messages, replace_function_messages};
pub(crate) use openllmetry::flattened_messages as openllmetry_messages;
pub(crate) use vercel::{assistant_message, vercel_messages};
