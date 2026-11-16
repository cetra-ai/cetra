//! The benchmark text corpus (track R M7, R7.2): OpenAssistant oasst2
//! (Apache-2.0), downloaded by `just fetch-corpus` and never committed.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

/// SHA-256 of the pinned oasst2 file (its LFS oid).
pub(crate) const CORPUS_SHA256: &str =
    "a9f240c4c77aa1378364f70d37e753c07ba284e247b019d700e1947a0e5da751";
/// File name under `target/bench-corpus`.
pub(crate) const CORPUS_FILE: &str = "oasst2-ready-messages.jsonl.gz";
/// Longest message text kept, in bytes.
pub(crate) const MAX_TEXT: usize = 4 * 1024;

/// `LLMOBS_BENCH_CORPUS`, else `<CARGO_MANIFEST_DIR>/../../target/bench-corpus/<CORPUS_FILE>`.
pub(crate) fn corpus_path() -> PathBuf {
    match std::env::var("LLMOBS_BENCH_CORPUS") {
        Ok(path) => PathBuf::from(path),
        Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/bench-corpus")
            .join(CORPUS_FILE),
    }
}

/// English message texts and the prompt/answer pairs among them.
pub(crate) struct Corpus {
    /// Message texts, each cut to at most `MAX_TEXT` bytes at a char boundary.
    texts: Vec<String>,
    /// (prompt index, answer index) into `texts`.
    turns: Vec<(u32, u32)>,
}

/// One message of the file. Serde ignores the other fields.
#[derive(serde::Deserialize)]
struct Line {
    message_id: String,
    parent_id: Option<String>,
    text: String,
    role: String,
    lang: String,
}

/// A kept English message.
struct Kept {
    id: String,
    parent: Option<String>,
    role: String,
    text: String,
}

impl Corpus {
    /// Reads the pinned file: checks its SHA-256 first, then keeps English
    /// messages and pairs each assistant message with its prompter parent.
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|error| format!("cannot read corpus: {error}"))?;
        if hex::encode(Sha256::digest(&bytes)) != CORPUS_SHA256 {
            return Err("corpus SHA-256 mismatch".to_owned());
        }
        let reader = BufReader::new(GzDecoder::new(bytes.as_slice()));
        let mut kept = Vec::new();
        for (number, line) in reader.lines().enumerate() {
            let line = line.map_err(|error| format!("corpus line {}: {error}", number + 1))?;
            let parsed: Line = serde_json::from_str(&line)
                .map_err(|_| format!("corpus line {}: not a message", number + 1))?;
            if parsed.lang == "en" && !parsed.text.is_empty() {
                kept.push(Kept {
                    id: parsed.message_id,
                    parent: parsed.parent_id,
                    role: parsed.role,
                    text: cut(&parsed.text, MAX_TEXT).to_owned(),
                });
            }
        }
        Self::from_kept(&kept)
    }

    fn from_kept(kept: &[Kept]) -> Result<Self, String> {
        // Message id → (text index, role). A text's index is its position in `kept`.
        let mut index: HashMap<&str, (u32, &str)> = HashMap::with_capacity(kept.len());
        let mut texts = Vec::with_capacity(kept.len());
        for message in kept {
            let at = u32::try_from(texts.len()).map_err(|_| "corpus too large".to_owned())?;
            index.insert(message.id.as_str(), (at, message.role.as_str()));
            texts.push(message.text.clone());
        }
        let mut turns = Vec::new();
        for (answer, message) in (0_u32..).zip(kept) {
            if message.role != "assistant" {
                continue;
            }
            let Some(parent) = message.parent.as_deref() else {
                continue;
            };
            if let Some(&(prompt, "prompter")) = index.get(parent) {
                turns.push((prompt, answer));
            }
        }
        if texts.len() < 50_000 || turns.len() < 30_000 {
            return Err(format!(
                "corpus has {} texts and {} turns: not the pinned file",
                texts.len(),
                turns.len()
            ));
        }
        Ok(Self { texts, turns })
    }

    /// `load(&corpus_path())`, panicking with the fix when the file is missing.
    pub(crate) fn from_env() -> Self {
        let path = corpus_path();
        assert!(
            path.is_file(),
            "corpus file missing: run `just fetch-corpus` or set LLMOBS_BENCH_CORPUS"
        );
        Self::load(&path).expect("the pinned corpus loads")
    }

    /// For tests: texts paired as turns (0, 1), (2, 3), …
    pub(crate) fn from_texts(texts: Vec<String>) -> Self {
        let turns = (0..texts.len() / 2)
            .map(|i| {
                (
                    u32::try_from(2 * i).expect("small test corpus"),
                    u32::try_from(2 * i + 1).expect("small test corpus"),
                )
            })
            .collect();
        Self { texts, turns }
    }

    pub(crate) fn text(&self, index: usize) -> &str {
        &self.texts[index]
    }

    pub(crate) fn texts(&self) -> usize {
        self.texts.len()
    }

    /// The prompt and answer texts of turn `index`.
    pub(crate) fn turn(&self, index: usize) -> (&str, &str) {
        let (prompt, answer) = self.turns[index];
        (&self.texts[prompt as usize], &self.texts[answer as usize])
    }

    pub(crate) fn turns(&self) -> usize {
        self.turns.len()
    }

    /// Total bytes of the kept texts.
    pub(crate) fn bytes(&self) -> usize {
        self.texts.iter().map(String::len).sum()
    }
}

impl std::fmt::Debug for Corpus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Corpus")
            .field("texts", &self.texts())
            .field("turns", &self.turns())
            .field("bytes", &self.bytes())
            .finish()
    }
}

/// `s` cut to at most `max` bytes at a char boundary.
pub(crate) fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
