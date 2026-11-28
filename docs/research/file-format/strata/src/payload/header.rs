//! The payload header of one column (S4 step 5): its row count and refs page table. The
//! column's chunks live in a payload store ([`super::store`]), which may be shared with other
//! columns; the directory record points to both.
//!
//! Body: `u32 n_rows` · `u32 n_pages` · per page `u64 offset`, `u32 len`. Page `i` holds rows
//! `[i × REFS_PAGE_ROWS, …)`.

use std::sync::Arc;

use super::REFS_PAGE_ROWS;
use super::store::PayloadStore;
use crate::error::{Result, StrataError};
use crate::format::{Cursor, Out};

/// Length of one refs page table entry.
const REF_PAGE_LEN: usize = 12;

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

/// The payload header of one column, with the store that holds its chunks.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PayloadHeader {
    /// Rows of the column.
    pub n_rows: u32,
    /// The refs pages, in row order.
    pub ref_pages: Vec<RefPage>,
    /// The store of the column's chunks.
    pub store: Arc<PayloadStore>,
}

/// Writes a header body for `n_rows` rows and the refs pages `pages`.
pub(crate) fn encode_header(n_rows: u32, pages: &[RefPage], out: &mut Out) -> Result<()> {
    out.u32(n_rows);
    out.u32(
        u32::try_from(pages.len()).map_err(|_| StrataError::Unsupported {
            what: "more than 4 billion refs pages".to_string(),
        })?,
    );
    for page in pages {
        out.u64(page.offset);
        out.u32(page.len);
    }
    Ok(())
}

impl PayloadHeader {
    /// Decodes and checks a header body. `offset` is the header segment's file offset.
    pub(crate) fn decode(bytes: &[u8], offset: u64, store: Arc<PayloadStore>) -> Result<Self> {
        let corrupt = |what: &'static str| StrataError::Corrupt { what, offset };
        let mut c = Cursor::new(bytes, offset);
        let n_rows = c.u32("payload header")?;
        let n_pages = c.u32("payload header")?;
        // One page per started block of rows; the length must match before allocating.
        if u64::from(n_pages) != u64::from(n_rows).div_ceil(REFS_PAGE_ROWS as u64) {
            return Err(corrupt("payload ref pages"));
        }
        if (n_pages as usize)
            .checked_mul(REF_PAGE_LEN)
            .and_then(|n| n.checked_add(8))
            != Some(bytes.len())
        {
            return Err(corrupt("payload header length"));
        }
        let mut ref_pages = Vec::with_capacity(n_pages as usize);
        for i in 0..n_pages {
            ref_pages.push(RefPage {
                offset: c.u64("payload ref page")?,
                len: c.u32("payload ref page")?,
                first_row: i * REFS_PAGE_ROWS as u32,
            });
        }
        Ok(Self {
            n_rows,
            ref_pages,
            store,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn store() -> Arc<PayloadStore> {
        Arc::new(PayloadStore {
            n_chunks: 0,
            frames: Vec::new(),
            runs: Vec::new(),
        })
    }

    fn page(i: u32) -> RefPage {
        RefPage {
            offset: 100 + u64::from(i) * 50,
            len: 40,
            first_row: i * REFS_PAGE_ROWS as u32,
        }
    }

    fn encoded(n_rows: u32, pages: &[RefPage]) -> Vec<u8> {
        let mut out = Out(Vec::new());
        encode_header(n_rows, pages, &mut out).unwrap();
        out.0
    }

    #[test]
    fn round_trip_and_exact_length() {
        let pages = vec![page(0), page(1)];
        let bytes = encoded(1_500, &pages);
        let h = PayloadHeader::decode(&bytes, 0, store()).unwrap();
        assert_eq!(h.n_rows, 1_500);
        assert_eq!(h.ref_pages, pages);
        assert!(PayloadHeader::decode(&bytes[..bytes.len() - 1], 0, store()).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(PayloadHeader::decode(&longer, 0, store()).is_err());
    }

    #[test]
    fn ref_pages_are_one_per_1024_rows() {
        assert!(PayloadHeader::decode(&encoded(1_025, &[page(0)]), 0, store()).is_err());
        assert!(PayloadHeader::decode(&encoded(1_025, &[page(0), page(1)]), 0, store()).is_ok());
        assert!(PayloadHeader::decode(&encoded(0, &[]), 0, store()).is_ok());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]

        #[test]
        fn random_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
            let _ = PayloadHeader::decode(&bytes, 0, store());
        }
    }
}
