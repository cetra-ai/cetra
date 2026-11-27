//! Selection vector module.

use crate::{Bitmap, ColumnError};

/// Active row filter attached to a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Every row `0..raw_len` is active in natural order.
    All,
    /// Bit `i == 1` means row `i` is active.
    Mask(Bitmap),
    /// Active row indices.
    Indices(Vec<u32>),
}

impl Selection {
    /// Validates `self` against a batch with `raw_len` physical rows.
    pub fn validate(&self, raw_len: usize) -> Result<(), ColumnError> {
        if raw_len > u32::MAX as usize {
            return Err(ColumnError::Overflow {
                reason: "raw length exceeds u32::MAX",
            });
        }
        match self {
            Self::All => Ok(()),
            Self::Mask(m) => {
                if m.len() != raw_len {
                    return Err(ColumnError::InvalidBatch {
                        reason: "selection mask length does not match raw length",
                    });
                }
                Ok(())
            }
            Self::Indices(idx) => {
                for &i in idx {
                    if (i as usize) >= raw_len {
                        return Err(ColumnError::InvalidBatch {
                            reason: "selection index out of bounds",
                        });
                    }
                }
                Ok(())
            }
        }
    }

    /// Number of active rows.
    pub fn active_len(&self, raw_len: usize) -> usize {
        match self {
            Self::All => raw_len,
            Self::Mask(m) => m.count_ones(),
            Self::Indices(idx) => idx.len(),
        }
    }

    /// True when no rows are active.
    pub fn is_empty(&self, raw_len: usize) -> bool {
        self.active_len(raw_len) == 0
    }

    /// True when every row `0..raw_len` is active in natural order.
    pub fn is_all(&self, raw_len: usize) -> bool {
        match self {
            Self::All => true,
            Self::Mask(m) => m.len() == raw_len && m.all_ones(),
            Self::Indices(idx) => {
                if idx.len() != raw_len {
                    return false;
                }
                idx.iter().enumerate().all(|(i, &v)| v as usize == i)
            }
        }
    }

    /// Materializes active row indices into a `Vec<u32>`.
    pub fn to_indices(&self, raw_len: usize) -> Result<Vec<u32>, ColumnError> {
        self.validate(raw_len)?;
        match self {
            Self::All => Ok((0..raw_len as u32).collect()),
            Self::Mask(m) => Ok(m.iter_ones().map(|i| i as u32).collect()),
            Self::Indices(idx) => Ok(idx.clone()),
        }
    }

    /// Slices `self` to physical row range `offset..offset + len`.
    pub fn slice(&self, raw_len: usize, offset: usize, len: usize) -> Result<Self, ColumnError> {
        self.validate(raw_len)?;
        if offset.checked_add(len).is_none_or(|end| end > raw_len) {
            return Err(ColumnError::InvalidBatch {
                reason: "selection slice out of bounds",
            });
        }
        match self {
            Self::All => Ok(Self::All),
            Self::Mask(m) => Ok(Self::Mask(m.slice(offset, len)?)),
            Self::Indices(idx) => {
                let end = offset + len;
                let sliced: Vec<u32> = idx
                    .iter()
                    .copied()
                    .filter_map(|i| {
                        let iu = i as usize;
                        if iu >= offset && iu < end {
                            Some((iu - offset) as u32)
                        } else {
                            None
                        }
                    })
                    .collect();
                Ok(Self::Indices(sliced))
            }
        }
    }

    /// Intersects `self` with an additional `mask`.
    pub fn intersect_mask(&self, raw_len: usize, mask: &Bitmap) -> Result<Self, ColumnError> {
        self.validate(raw_len)?;
        if mask.len() != raw_len {
            return Err(ColumnError::InvalidBatch {
                reason: "intersect mask length mismatch",
            });
        }
        match self {
            Self::All => Ok(Self::Mask(mask.clone())),
            Self::Mask(m) => Ok(Self::Mask(m.and(mask)?)),
            Self::Indices(idx) => {
                let retained: Vec<u32> = idx
                    .iter()
                    .copied()
                    .filter(|&i| mask.get(i as usize))
                    .collect();
                Ok(Self::Indices(retained))
            }
        }
    }

    /// Deterministic logical heap bytes.
    pub fn memory_bytes(&self) -> usize {
        match self {
            Self::All => 0,
            Self::Mask(m) => m.memory_bytes(),
            Self::Indices(v) => v.len() * 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_slice_and_intersect_mask() {
        let all = Selection::All;
        assert!(all.is_all(10));
        assert_eq!(all.active_len(10), 10);
        assert_eq!(all.to_indices(5).unwrap(), vec![0, 1, 2, 3, 4]);

        let mask = Bitmap::from_bools(&[false, true, false, true, true]);
        let sel = Selection::Mask(mask.clone());
        assert!(!sel.is_all(5));
        assert_eq!(sel.active_len(5), 3);
        assert_eq!(sel.to_indices(5).unwrap(), vec![1, 3, 4]);
        assert_eq!(
            sel.slice(5, 1, 3).unwrap().to_indices(3).unwrap(),
            vec![0, 2]
        );

        let idx = Selection::Indices(vec![4, 1, 3, 1]);
        assert!(idx.validate(5).is_ok());
        assert!(idx.validate(4).is_err());
        assert!(!idx.is_all(5));
        assert!(Selection::Indices(vec![0, 1, 2]).is_all(3));
        assert_eq!(
            idx.slice(5, 1, 3).unwrap(),
            Selection::Indices(vec![0, 2, 0])
        );
        let inter = idx.intersect_mask(5, &mask).unwrap();
        assert_eq!(inter, Selection::Indices(vec![4, 1, 3, 1]));
        let mask2 = Bitmap::from_bools(&[true, false, true, true, false]);
        assert_eq!(
            idx.intersect_mask(5, &mask2).unwrap(),
            Selection::Indices(vec![3])
        );
        assert!(idx.intersect_mask(5, &Bitmap::new_ones(4)).is_err());
    }
}
