//! The payload header (S4 step 5): the counts, the refs page table, the edge group table and
//! the frame table of one payload column. The header is stored as one `PayloadHeader`
//! segment; this module encodes and decodes its body.

use super::frames::FrameEntry;
use super::{EDGE_GROUP_CHUNKS, HEADER_FIXED_LEN, REFS_PAGE_ROWS};
use crate::error::{Result, StrataError};
use crate::format::{Cursor, Out};

/// Length of one refs page table entry.
const REF_PAGE_LEN: usize = 16;
/// Length of one edge group table entry.
const EDGE_GROUP_LEN: usize = 16;
/// Length of one frame table entry.
const FRAME_LEN: usize = 24;

/// One refs page of the column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefPage {
    /// Absolute file offset of the page segment.
    pub offset: u64,
    /// Length of the page segment, header included.
    pub len: u32,
    /// Row of the page's first row.
    pub first_row: u32,
}

/// One edge group: the tails of up to `EDGE_GROUP_CHUNKS` consecutive chunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeGroup {
    /// Absolute file offset of the group segment.
    pub offset: u64,
    /// Length of the group segment, header included.
    pub len: u32,
    /// Id of the group's first chunk.
    pub first_chunk: u32,
}

/// The payload header of one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PayloadHeader {
    /// Rows of the column.
    pub n_rows: u32,
    /// Unique chunks of the column.
    pub n_chunks: u32,
    /// Offset of the filters segment.
    pub filters_offset: u64,
    /// Length of the filters segment, header included.
    pub filters_len: u32,
    /// The refs pages, in row order.
    pub ref_pages: Vec<RefPage>,
    /// The edge groups, in chunk order: group `i` starts at chunk `i × EDGE_GROUP_CHUNKS`.
    pub edge_groups: Vec<EdgeGroup>,
    /// The frames, in chunk order.
    pub frames: Vec<FrameEntry>,
}

impl PayloadHeader {
    /// Writes the header body.
    pub(crate) fn encode(&self, out: &mut Out) -> Result<()> {
        let count = |n: usize| {
            u32::try_from(n).map_err(|_| StrataError::Unsupported {
                what: "more than 4 billion payload entries".to_string(),
            })
        };
        out.u32(self.n_rows);
        out.u32(count(self.frames.len())?);
        out.u32(self.n_chunks);
        out.u32(count(self.ref_pages.len())?);
        out.u32(count(self.edge_groups.len())?);
        out.u64(self.filters_offset);
        out.u32(self.filters_len);
        for page in &self.ref_pages {
            out.u64(page.offset);
            out.u32(page.len);
            out.u32(page.first_row);
        }
        for group in &self.edge_groups {
            out.u64(group.offset);
            out.u32(group.len);
            out.u32(group.first_chunk);
        }
        for frame in &self.frames {
            out.u64(frame.offset);
            out.u32(frame.len);
            out.u32(frame.raw_len);
            out.u32(frame.first_chunk);
            out.u32(frame.n_chunks);
        }
        Ok(())
    }

    /// Decodes and checks a header body. `offset` is the header segment's file offset.
    pub(crate) fn decode(bytes: &[u8], offset: u64) -> Result<Self> {
        let corrupt = |what: &'static str| StrataError::Corrupt { what, offset };
        let mut c = Cursor::new(bytes, offset);
        let n_rows = c.u32("payload header")?;
        let n_frames = c.u32("payload header")?;
        let n_chunks = c.u32("payload header")?;
        let n_refs = c.u32("payload header")?;
        let n_edges = c.u32("payload header")?;
        let filters_offset = c.u64("payload header")?;
        let filters_len = c.u32("payload header")?;

        // The length must match the counts exactly, checked before anything is allocated.
        let expected = (n_refs as usize)
            .checked_mul(REF_PAGE_LEN)
            .and_then(|refs| {
                (n_edges as usize)
                    .checked_mul(EDGE_GROUP_LEN)
                    .and_then(|edges| refs.checked_add(edges))
            })
            .and_then(|sum| {
                (n_frames as usize)
                    .checked_mul(FRAME_LEN)
                    .and_then(|frames| sum.checked_add(frames))
            })
            .and_then(|sum| sum.checked_add(HEADER_FIXED_LEN))
            .ok_or_else(|| corrupt("payload header length"))?;
        if expected != bytes.len() {
            return Err(corrupt("payload header length"));
        }

        let mut ref_pages = Vec::with_capacity(n_refs as usize);
        for _ in 0..n_refs {
            ref_pages.push(RefPage {
                offset: c.u64("payload ref page")?,
                len: c.u32("payload ref page")?,
                first_row: c.u32("payload ref page")?,
            });
        }
        let mut edge_groups = Vec::with_capacity(n_edges as usize);
        for _ in 0..n_edges {
            edge_groups.push(EdgeGroup {
                offset: c.u64("payload edge group")?,
                len: c.u32("payload edge group")?,
                first_chunk: c.u32("payload edge group")?,
            });
        }
        let mut frames = Vec::with_capacity(n_frames as usize);
        for _ in 0..n_frames {
            frames.push(FrameEntry {
                offset: c.u64("payload frame")?,
                len: c.u32("payload frame")?,
                raw_len: c.u32("payload frame")?,
                first_chunk: c.u32("payload frame")?,
                n_chunks: c.u32("payload frame")?,
            });
        }

        // Frames are non-empty and contiguous in chunk ids, and cover every chunk.
        let mut next_chunk = 0u32;
        for frame in &frames {
            if frame.n_chunks == 0 || frame.first_chunk != next_chunk {
                return Err(corrupt("payload frame table"));
            }
            next_chunk = next_chunk
                .checked_add(frame.n_chunks)
                .ok_or_else(|| corrupt("payload frame table"))?;
        }
        if next_chunk != n_chunks {
            return Err(corrupt("payload chunk count"));
        }

        // One edge group per EDGE_GROUP_CHUNKS chunks, each starting at its own multiple.
        if edge_groups.len() != (n_chunks as usize).div_ceil(EDGE_GROUP_CHUNKS) {
            return Err(corrupt("payload edge groups"));
        }
        for (i, group) in edge_groups.iter().enumerate() {
            let first = (i as u64) * (EDGE_GROUP_CHUNKS as u64);
            if u64::from(group.first_chunk) != first {
                return Err(corrupt("payload edge groups"));
            }
        }

        // Every refs page holds exactly REFS_PAGE_ROWS rows (the last one fewer), so there is one
        // page per started block of rows, and page `i` starts at row `i × REFS_PAGE_ROWS`. The
        // arithmetic is in u64, so no row count can wrap.
        let pages_needed = u64::from(n_rows).div_ceil(REFS_PAGE_ROWS as u64);
        if ref_pages.len() as u64 != pages_needed {
            return Err(corrupt("payload ref pages"));
        }
        for (i, page) in ref_pages.iter().enumerate() {
            if u64::from(page.first_row) != (i as u64) * (REFS_PAGE_ROWS as u64) {
                return Err(corrupt("payload ref pages"));
            }
        }

        Ok(Self {
            n_rows,
            n_chunks,
            filters_offset,
            filters_len,
            ref_pages,
            edge_groups,
            frames,
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
    use proptest::prelude::*;

    fn sample() -> PayloadHeader {
        PayloadHeader {
            n_rows: 5000,
            n_chunks: 3,
            filters_offset: 0,
            filters_len: 0,
            ref_pages: vec![
                RefPage {
                    offset: 166,
                    len: 40,
                    first_row: 0,
                },
                RefPage {
                    offset: 206,
                    len: 30,
                    first_row: 4096,
                },
            ],
            edge_groups: vec![EdgeGroup {
                offset: 300,
                len: 40,
                first_chunk: 0,
            }],
            frames: vec![
                FrameEntry {
                    offset: 16,
                    len: 100,
                    raw_len: 200,
                    first_chunk: 0,
                    n_chunks: 2,
                },
                FrameEntry {
                    offset: 116,
                    len: 50,
                    raw_len: 90,
                    first_chunk: 2,
                    n_chunks: 1,
                },
            ],
        }
    }

    fn encoded(h: &PayloadHeader) -> Vec<u8> {
        let mut out = Out(Vec::new());
        h.encode(&mut out).unwrap();
        out.0
    }

    fn is_corrupt<T: std::fmt::Debug>(result: Result<T>) -> bool {
        matches!(result, Err(StrataError::Corrupt { .. }))
    }

    #[test]
    fn round_trip_and_exact_length() {
        let h = sample();
        let bytes = encoded(&h);
        // 32 fixed bytes, two 16-byte refs entries, one 16-byte edge group entry, two 24-byte
        // frame entries.
        assert_eq!(bytes.len(), 32 + 2 * 16 + 16 + 2 * 24);
        assert_eq!(PayloadHeader::decode(&bytes, 0).unwrap(), h);
    }

    /// A header of `n_chunks` chunks in one frame, with `groups` edge groups laid out as the
    /// writer lays them out.
    fn with_chunks(n_chunks: u32, groups: usize) -> PayloadHeader {
        PayloadHeader {
            n_rows: 0,
            n_chunks,
            filters_offset: 0,
            filters_len: 0,
            ref_pages: Vec::new(),
            edge_groups: (0..groups)
                .map(|i| EdgeGroup {
                    offset: 300 + 40 * i as u64,
                    len: 40,
                    first_chunk: (i * EDGE_GROUP_CHUNKS) as u32,
                })
                .collect(),
            frames: if n_chunks == 0 {
                Vec::new()
            } else {
                vec![FrameEntry {
                    offset: 16,
                    len: 100,
                    raw_len: 200,
                    first_chunk: 0,
                    n_chunks,
                }]
            },
        }
    }

    #[test]
    fn edge_group_counts_round_trip() {
        for (n_chunks, groups) in [(0, 0), (1024, 1), (1025, 2)] {
            let h = with_chunks(n_chunks, groups);
            assert_eq!(PayloadHeader::decode(&encoded(&h), 0).unwrap(), h);
        }
    }

    #[test]
    fn wrong_edge_group_count_is_corrupt() {
        let mut h = with_chunks(1025, 2);
        h.edge_groups.pop();
        assert!(is_corrupt(PayloadHeader::decode(&encoded(&h), 0)));
        let mut h = with_chunks(1024, 1);
        h.edge_groups.push(EdgeGroup {
            offset: 400,
            len: 40,
            first_chunk: 1024,
        });
        assert!(is_corrupt(PayloadHeader::decode(&encoded(&h), 0)));
        assert!(is_corrupt(PayloadHeader::decode(
            &encoded(&with_chunks(0, 1)),
            0
        )));
    }

    #[test]
    fn wrong_edge_group_first_chunk_is_corrupt() {
        let mut h = with_chunks(1025, 2);
        h.edge_groups[1].first_chunk = 1023;
        assert!(is_corrupt(PayloadHeader::decode(&encoded(&h), 0)));
    }

    #[test]
    fn non_contiguous_first_chunk_is_corrupt() {
        let mut h = sample();
        h.frames[1].first_chunk = 3;
        assert!(is_corrupt(PayloadHeader::decode(&encoded(&h), 0)));
    }

    #[test]
    fn empty_frame_is_corrupt() {
        let mut h = sample();
        h.frames[1].n_chunks = 0;
        h.n_chunks = 2;
        assert!(is_corrupt(PayloadHeader::decode(&encoded(&h), 0)));
    }

    #[test]
    fn truncated_header_is_corrupt() {
        let bytes = encoded(&sample());
        assert!(is_corrupt(PayloadHeader::decode(
            &bytes[..bytes.len() - 1],
            0
        )));
        assert!(is_corrupt(PayloadHeader::decode(&bytes[..31], 0)));
    }

    #[test]
    fn ref_pages_must_start_at_zero_and_increase() {
        let mut h = sample();
        h.ref_pages[0].first_row = 1;
        assert!(is_corrupt(PayloadHeader::decode(&encoded(&h), 0)));
        let mut h = sample();
        h.ref_pages[1].first_row = 0;
        assert!(is_corrupt(PayloadHeader::decode(&encoded(&h), 0)));
    }

    #[test]
    fn ref_pages_are_one_per_4096_rows() {
        let page = |first_row: u32| RefPage {
            offset: 166,
            len: 40,
            first_row,
        };
        let with_pages = |n_rows: u32, ref_pages: Vec<RefPage>| PayloadHeader {
            n_rows,
            n_chunks: 0,
            filters_offset: 0,
            filters_len: 0,
            ref_pages,
            edge_groups: Vec::new(),
            frames: Vec::new(),
        };
        // 4,097 rows need two pages, at rows 0 and 4,096.
        assert!(is_corrupt(PayloadHeader::decode(
            &encoded(&with_pages(4_097, vec![page(0)])),
            0
        )));
        assert!(
            PayloadHeader::decode(&encoded(&with_pages(4_097, vec![page(0), page(4_096)])), 0)
                .is_ok()
        );
        assert!(is_corrupt(PayloadHeader::decode(
            &encoded(&with_pages(4_097, vec![page(0), page(4_000)])),
            0
        )));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]

        #[test]
        fn random_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
            let _ = PayloadHeader::decode(&bytes, 0);
        }
    }
}
