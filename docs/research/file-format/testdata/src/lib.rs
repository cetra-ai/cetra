//! Test data for the file-format research: synthetic LLM-shaped spans
//! (`datagen`), SWE-agent trajectories converted to payload values (`swe`), and
//! public conversation datasets converted by `bench/convert_convos.py` (`convo`),
//! and FineWeb text cut into pieces for the Braintrust comparison (`fineweb`).

pub mod datagen;
pub mod convo;
pub mod fineweb;
pub mod swe;
