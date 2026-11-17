//! Dedup scope measurement (survey experiment E2). Runs the writer's chunker and frame builder
//! over several payload columns twice: with one chunk store per column, as the writer does, and
//! with one store shared by all the columns. Nothing is written to a file.

use std::collections::HashMap;

use xxhash_rust::xxh3::xxh3_128;

use super::FRAME_ZSTD_LEVEL;
use super::chunk::chunk_value;
use super::frames::FrameBuilder;
use crate::error::Result;
use crate::format::Out;
use crate::segment::SegmentCodec;

/// One column's figures.
#[derive(Debug, Clone, Default)]
pub struct ScopeColumn {
    /// Column name.
    pub name: String,
    /// Sum of the non-null value lengths.
    pub raw_bytes: u64,
    /// Chunk references (one per chunk of every value).
    pub chunk_refs: u64,
    /// Unique chunks with a store of its own.
    pub own_chunks: u64,
    /// Raw bytes of those unique chunks.
    pub own_unique_bytes: u64,
    /// Frame bytes with a store of its own.
    pub own_frame_bytes: u64,
    /// Raw bytes of this column's unique chunks that some other column also holds.
    pub shared_with_others_bytes: u64,
    /// Raw bytes this column adds to the shared store: chunks no earlier row or column held.
    pub new_in_shared_bytes: u64,
}

/// The result of [`DedupScope::finish`].
#[derive(Debug, Clone, Default)]
pub struct ScopeReport {
    /// Per column, in the order given to [`DedupScope::new`].
    pub columns: Vec<ScopeColumn>,
    /// Unique chunks of the shared store.
    pub shared_chunks: u64,
    /// Raw bytes of the shared store's unique chunks.
    pub shared_unique_bytes: u64,
    /// Frame bytes of the shared store.
    pub shared_frame_bytes: u64,
}

/// Feeds rows to one store per column and to one shared store.
pub struct DedupScope {
    columns: Vec<ScopeColumn>,
    own: Vec<FrameBuilder>,
    own_hashes: Vec<HashMap<u128, u32>>,
    shared: FrameBuilder,
    shared_unique_bytes: u64,
    codec: SegmentCodec,
    own_out: Out,
    shared_out: Out,
}

impl DedupScope {
    /// A measurement over columns with these names.
    pub fn new(names: &[&str]) -> Result<Self> {
        Ok(Self {
            columns: names
                .iter()
                .map(|n| ScopeColumn { name: (*n).to_owned(), ..ScopeColumn::default() })
                .collect(),
            own: names.iter().map(|_| FrameBuilder::new()).collect(),
            own_hashes: names.iter().map(|_| HashMap::new()).collect(),
            shared: FrameBuilder::new(),
            shared_unique_bytes: 0,
            codec: SegmentCodec::new(FRAME_ZSTD_LEVEL)?,
            own_out: Out(Vec::new()),
            shared_out: Out(Vec::new()),
        })
    }

    /// Adds one row: one value per column, in the order of the names. The shared store sees
    /// the columns in that order, so an earlier column owns a chunk both hold.
    pub fn add_row(&mut self, values: &[Option<&[u8]>]) -> Result<()> {
        for (c, value) in values.iter().enumerate() {
            let Some(bytes) = value else { continue };
            let (Some(col), Some(own), Some(hashes)) =
                (self.columns.get_mut(c), self.own.get_mut(c), self.own_hashes.get_mut(c))
            else {
                continue;
            };
            col.raw_bytes += bytes.len() as u64;
            for range in chunk_value(bytes) {
                let chunk = bytes.get(range).unwrap_or_default();
                col.chunk_refs += 1;
                let hash = xxh3_128(chunk);
                if !own.has(hash) {
                    col.own_chunks += 1;
                    col.own_unique_bytes += chunk.len() as u64;
                    hashes.insert(hash, u32::try_from(chunk.len()).unwrap_or(u32::MAX));
                }
                own.add(chunk, &mut self.codec, &mut self.own_out)?;
                if !self.shared.has(hash) {
                    col.new_in_shared_bytes += chunk.len() as u64;
                    self.shared_unique_bytes += chunk.len() as u64;
                }
                self.shared.add(chunk, &mut self.codec, &mut self.shared_out)?;
            }
            // The frame bytes are only needed as totals: drop what was written.
            self.own_out.0.clear();
            self.shared_out.0.clear();
        }
        Ok(())
    }

    /// Closes the open frames and returns the figures.
    pub fn finish(mut self) -> Result<ScopeReport> {
        for (c, own) in self.own.iter_mut().enumerate() {
            own.finish(&mut self.codec, &mut self.own_out)?;
            if let Some(col) = self.columns.get_mut(c) {
                col.own_frame_bytes = own.stored_bytes;
            }
        }
        self.shared.finish(&mut self.codec, &mut self.shared_out)?;
        for c in 0..self.columns.len() {
            let mut bytes = 0u64;
            if let Some(mine) = self.own_hashes.get(c) {
                for (hash, len) in mine {
                    let elsewhere = self
                        .own_hashes
                        .iter()
                        .enumerate()
                        .any(|(o, other)| o != c && other.contains_key(hash));
                    if elsewhere {
                        bytes += u64::from(*len);
                    }
                }
            }
            if let Some(col) = self.columns.get_mut(c) {
                col.shared_with_others_bytes = bytes;
            }
        }
        Ok(ScopeReport {
            columns: self.columns,
            shared_chunks: u64::from(self.shared.n_chunks()),
            shared_unique_bytes: self.shared_unique_bytes,
            shared_frame_bytes: self.shared.stored_bytes,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    /// Non-repeating text, so chunks only match where the bytes really repeat.
    fn prose(words: usize, seed: u64) -> String {
        const VOCAB: [&str; 8] = ["alpha", "beta", "gamma", "delta", "kappa", "omega", "sigma", "tau"];
        let mut x = seed;
        let mut s = String::new();
        for _ in 0..words {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            s.push_str(VOCAB[(x % 8) as usize]);
            s.push_str(&(x % 1000).to_string());
            s.push(' ');
        }
        s
    }

    #[test]
    fn an_output_resent_in_the_next_input_is_stored_once_when_shared() {
        let answer = prose(10_000, 7);
        let output = serde_json::json!([{"role":"assistant","parts":[{"type":"text","content":answer}],"finish_reason":"stop"}]).to_string();
        let input = serde_json::json!([
            {"role":"user","parts":[{"type":"text","content":"question"}]},
            {"role":"assistant","parts":[{"type":"text","content":answer}]},
            {"role":"user","parts":[{"type":"text","content":"thanks"}]}
        ])
        .to_string();
        let mut scope = DedupScope::new(&["output", "input"]).unwrap();
        scope.add_row(&[Some(output.as_bytes()), None]).unwrap();
        scope.add_row(&[None, Some(input.as_bytes())]).unwrap();
        let r = scope.finish().unwrap();
        let own_unique: u64 = r.columns.iter().map(|c| c.own_unique_bytes).sum();
        assert!(r.shared_unique_bytes < own_unique);
        let [out, inp] = &r.columns[..] else { panic!("two columns") };
        // Only the chunks around the differing envelope are new in the input.
        assert!(inp.new_in_shared_bytes * 4 < inp.own_unique_bytes);
        assert!(out.shared_with_others_bytes * 4 > out.own_unique_bytes * 3);
        assert_eq!(out.new_in_shared_bytes, out.own_unique_bytes);
        assert!(r.shared_frame_bytes > 0);
    }

    #[test]
    fn unrelated_columns_share_nothing() {
        let mut scope = DedupScope::new(&["a", "b"]).unwrap();
        let a = prose(300, 1);
        let b = prose(300, 2);
        scope.add_row(&[Some(a.as_bytes()), Some(b.as_bytes())]).unwrap();
        let r = scope.finish().unwrap();
        assert_eq!(r.columns[0].shared_with_others_bytes, 0);
        assert_eq!(r.columns[1].shared_with_others_bytes, 0);
        assert_eq!(
            r.shared_unique_bytes,
            r.columns[0].own_unique_bytes + r.columns[1].own_unique_bytes
        );
    }
}
