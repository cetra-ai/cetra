//! Chunk postings: for a dictionary-encoded column, the chunks that hold each bucket of codes.
//!
//! An equality or `IN` predicate on the column reads only the chunks the postings list for its
//! codes, instead of every chunk. Codes are grouped into buckets of `2^shift` consecutive codes,
//! so the offsets stay small; a bucket's list is the union of its codes' chunks, a superset.
//!
//! Layout: `u32 n_codes` · `u8 shift` · `u32 n_buckets` · `u32 offsets[n_buckets + 1]` (byte
//! offsets into the lists) · the lists: each bucket's chunk indexes, ascending, as LEB128
//! deltas from the previous index (the first from 0).

use std::collections::BTreeSet;

use crate::error::{Result, StrataError};
use crate::format::{Cursor, Out};

/// Buckets of codes at most this many.
const MAX_BUCKETS: usize = 4_096;

/// The chunk lists of one column, by bucket of codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChunkPostings {
    n_codes: u32,
    shift: u8,
    offsets: Vec<u32>,
    lists: Vec<u8>,
}

/// The smallest shift that gives at most `MAX_BUCKETS` buckets for `n_codes` codes.
fn shift_for(n_codes: usize) -> u8 {
    let mut shift = 0u8;
    while n_codes.div_ceil(1usize << shift) > MAX_BUCKETS {
        shift += 1;
    }
    shift
}

fn push_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

impl ChunkPostings {
    /// Postings for a column of `n_codes` codes whose chunk `i` holds the codes `chunks[i]`
    /// (any order, repeats allowed). A code at or above `n_codes` is an error.
    pub(crate) fn build(n_codes: usize, chunks: &[Vec<u32>]) -> Result<ChunkPostings> {
        let out_of_range = || StrataError::Schema {
            reason: "internal: chunk code outside the dictionary".to_string(),
        };
        let shift = shift_for(n_codes);
        let n_buckets = n_codes.div_ceil(1usize << shift);
        let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); n_buckets];
        for (chunk, codes) in chunks.iter().enumerate() {
            let chunk = u32::try_from(chunk).map_err(|_| out_of_range())?;
            for &code in codes {
                if code as usize >= n_codes {
                    return Err(out_of_range());
                }
                let list = buckets
                    .get_mut((code >> shift) as usize)
                    .ok_or_else(out_of_range)?;
                if list.last() != Some(&chunk) {
                    list.push(chunk);
                }
            }
        }
        let mut offsets = Vec::with_capacity(n_buckets + 1);
        let mut lists = Vec::new();
        for list in &buckets {
            offsets.push(u32::try_from(lists.len()).map_err(|_| out_of_range())?);
            let mut previous = 0u32;
            for &chunk in list {
                push_varint(&mut lists, chunk - previous);
                previous = chunk;
            }
        }
        offsets.push(u32::try_from(lists.len()).map_err(|_| out_of_range())?);
        Ok(ChunkPostings {
            n_codes: u32::try_from(n_codes).map_err(|_| out_of_range())?,
            shift,
            offsets,
            lists,
        })
    }

    /// Appends the layout described in the module documentation.
    pub(crate) fn encode(&self, out: &mut Out) {
        out.u32(self.n_codes);
        out.u8(self.shift);
        out.u32(self.offsets.len().saturating_sub(1) as u32);
        for offset in &self.offsets {
            out.u32(*offset);
        }
        out.bytes(&self.lists);
    }

    /// Decodes postings written by [`ChunkPostings::encode`] for a column of `n_chunks`
    /// chunks, checking the shift, the offsets and every chunk index. `offset` is used only in
    /// error messages.
    pub(crate) fn decode(bytes: &[u8], n_chunks: usize, offset: u64) -> Result<ChunkPostings> {
        let corrupt = || StrataError::Corrupt {
            what: "chunk postings",
            offset,
        };
        let mut c = Cursor::new(bytes, offset);
        let n_codes = c.u32("chunk postings")?;
        let shift = c.u8("chunk postings")?;
        let n_buckets = c.u32("chunk postings")? as usize;
        if shift > 31 || n_buckets != (n_codes as usize).div_ceil(1usize << shift) {
            return Err(corrupt());
        }
        if n_buckets > MAX_BUCKETS {
            return Err(corrupt());
        }
        let mut offsets = Vec::with_capacity(n_buckets + 1);
        for _ in 0..=n_buckets {
            offsets.push(c.u32("chunk postings")?);
        }
        let rest = c.remaining();
        let lists = c.bytes(rest, "chunk postings")?.to_vec();
        if offsets.first() != Some(&0)
            || offsets.last().map(|&o| o as usize) != Some(lists.len())
            || offsets.windows(2).any(|w| w.first() > w.get(1))
        {
            return Err(corrupt());
        }
        let postings = ChunkPostings {
            n_codes,
            shift,
            offsets,
            lists,
        };
        // Every list must decode to ascending chunk indexes below `n_chunks`.
        for bucket in 0..n_buckets {
            postings.read_list(bucket, n_chunks, offset, |_| {})?;
        }
        Ok(postings)
    }

    /// Calls `f` with the chunk indexes of `bucket`, ascending.
    fn read_list(
        &self,
        bucket: usize,
        n_chunks: usize,
        offset: u64,
        mut f: impl FnMut(u32),
    ) -> Result<()> {
        let corrupt = || StrataError::Corrupt {
            what: "chunk postings",
            offset,
        };
        let start = *self.offsets.get(bucket).ok_or_else(corrupt)? as usize;
        let end = *self.offsets.get(bucket + 1).ok_or_else(corrupt)? as usize;
        let list = self.lists.get(start..end).ok_or_else(corrupt)?;
        let (mut at, mut chunk, mut first) = (0usize, 0u32, true);
        while at < list.len() {
            let (mut delta, mut shift) = (0u32, 0u32);
            loop {
                let byte = *list.get(at).ok_or_else(corrupt)?;
                at += 1;
                if shift > 28 {
                    return Err(corrupt());
                }
                delta |= u32::from(byte & 0x7f) << shift;
                shift += 7;
                if byte & 0x80 == 0 {
                    break;
                }
            }
            if !first && delta == 0 {
                return Err(corrupt());
            }
            chunk = chunk.checked_add(delta).ok_or_else(corrupt)?;
            if chunk as usize >= n_chunks {
                return Err(corrupt());
            }
            first = false;
            f(chunk);
        }
        Ok(())
    }

    /// The chunks that may hold one of `codes`. Codes at or above the dictionary's length
    /// select nothing.
    pub(crate) fn chunks_for(&self, codes: &BTreeSet<u32>, n_chunks: usize) -> Result<Vec<u32>> {
        let mut buckets: BTreeSet<usize> = BTreeSet::new();
        for &code in codes {
            if code < self.n_codes {
                buckets.insert((code >> self.shift) as usize);
            }
        }
        let mut chunks = BTreeSet::new();
        for bucket in buckets {
            self.read_list(bucket, n_chunks, 0, |chunk| {
                chunks.insert(chunk);
            })?;
        }
        Ok(chunks.into_iter().collect())
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

    #[test]
    fn postings_round_trip_and_answer_codes() {
        let chunks = vec![vec![0, 5, 5], vec![7], vec![], vec![5, 9_999], vec![0]];
        let p = ChunkPostings::build(10_000, &chunks).unwrap();
        let mut out = Out(Vec::new());
        p.encode(&mut out);
        let back = ChunkPostings::decode(&out.0, chunks.len(), 0).unwrap();
        assert_eq!(back, p);
        let set = |codes: &[u32]| codes.iter().copied().collect::<BTreeSet<u32>>();
        // Every chunk holding a code is returned; others may be, through a shared bucket.
        for (codes, must) in [
            (set(&[5]), vec![0u32, 3]),
            (set(&[7]), vec![1]),
            (set(&[9_999]), vec![3]),
            (set(&[0, 7]), vec![0, 1, 4]),
        ] {
            let got = back.chunks_for(&codes, chunks.len()).unwrap();
            for c in &must {
                assert!(got.contains(c), "{codes:?}: {got:?} misses {c}");
            }
            for c in &got {
                let holds_bucket = chunks[*c as usize]
                    .iter()
                    .any(|k| codes.iter().any(|q| k >> p.shift == q >> p.shift));
                assert!(
                    holds_bucket,
                    "{codes:?}: chunk {c} holds no code of the buckets"
                );
            }
        }
        assert!(
            back.chunks_for(&set(&[20_000]), chunks.len())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn buckets_stay_bounded() {
        assert_eq!(shift_for(0), 0);
        assert_eq!(shift_for(4_096), 0);
        assert_eq!(shift_for(4_097), 1);
        assert!(1_048_576usize.div_ceil(1 << shift_for(1_048_576)) <= MAX_BUCKETS);
    }

    #[test]
    fn corrupt_postings_are_errors() {
        let p = ChunkPostings::build(100, &[vec![1], vec![2, 3]]).unwrap();
        let mut out = Out(Vec::new());
        p.encode(&mut out);
        // A chunk index past the column's chunks.
        assert!(ChunkPostings::decode(&out.0, 1, 0).is_err());
        // Truncated.
        assert!(ChunkPostings::decode(&out.0[..out.0.len() - 1], 2, 0).is_err());
        // A wrong bucket count.
        let mut bad = out.0.clone();
        bad[5] ^= 1;
        assert!(ChunkPostings::decode(&bad, 2, 0).is_err());
    }
}
