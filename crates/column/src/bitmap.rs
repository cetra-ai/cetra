//! Packed bitset module.

use crate::ColumnError;

/// Packed bitset storing `len` bits in little-endian `u64` words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    words: Vec<u64>,
    len: usize,
}

fn clear_trailing_bits(words: &mut [u64], len: usize) {
    if len == 0 {
        for w in words.iter_mut() {
            *w = 0;
        }
        return;
    }
    let rem = len % 64;
    if rem != 0
        && let Some(last) = words.last_mut()
    {
        let mask = (1u64 << rem) - 1;
        *last &= mask;
    }
}

impl Bitmap {
    /// Creates an all-zero bitmap of `len` bits.
    pub fn new_zeroed(len: usize) -> Self {
        let words_len = len.div_ceil(64);
        Self {
            words: vec![0; words_len],
            len,
        }
    }

    /// Creates an all-one bitmap of `len` bits.
    pub fn new_ones(len: usize) -> Self {
        let words_len = len.div_ceil(64);
        let mut words = vec![!0u64; words_len];
        clear_trailing_bits(&mut words, len);
        Self { words, len }
    }

    /// Builds a bitmap from a slice of booleans.
    pub fn from_bools(bools: &[bool]) -> Self {
        let len = bools.len();
        let words_len = len.div_ceil(64);
        let mut words = vec![0u64; words_len];
        for (i, &b) in bools.iter().enumerate() {
            if b {
                words[i / 64] |= 1u64 << (i % 64);
            }
        }
        clear_trailing_bits(&mut words, len);
        Self { words, len }
    }

    /// Builds a bitmap from raw `u64` words.
    pub fn from_words(words: Vec<u64>, len: usize) -> Result<Self, ColumnError> {
        if words.len() != len.div_ceil(64) {
            return Err(ColumnError::InvalidBatch {
                reason: "bitmap word count does not match length",
            });
        }
        let mut w = words;
        clear_trailing_bits(&mut w, len);
        Ok(Self { words: w, len })
    }

    /// Builds a bitmap from little-endian bytes.
    pub fn from_bytes(bytes: &[u8], len: usize) -> Result<Self, ColumnError> {
        if bytes.len() != len.div_ceil(8) {
            return Err(ColumnError::InvalidBatch {
                reason: "bitmap byte count does not match length",
            });
        }
        let words_len = len.div_ceil(64);
        let mut words = vec![0u64; words_len];
        for (i, &b) in bytes.iter().enumerate() {
            let word_idx = i / 8;
            let byte_shift = (i % 8) * 8;
            if word_idx < words.len() {
                words[word_idx] |= (b as u64) << byte_shift;
            }
        }
        clear_trailing_bits(&mut words, len);
        Ok(Self { words, len })
    }

    /// Serializes to little-endian bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let byte_len = self.len.div_ceil(8);
        let mut bytes = vec![0u8; byte_len];
        for (i, b) in bytes.iter_mut().enumerate() {
            let word_idx = i / 8;
            let byte_shift = (i % 8) * 8;
            if word_idx < self.words.len() {
                *b = (self.words[word_idx] >> byte_shift) as u8;
            }
        }
        bytes
    }

    /// Total number of bits.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when `len == 0`.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns bit at `index`.
    pub fn get(&self, index: usize) -> bool {
        if index >= self.len {
            return false;
        }
        (self.words[index / 64] & (1u64 << (index % 64))) != 0
    }

    /// Sets bit at `index` to `value`.
    pub fn set(&mut self, index: usize, value: bool) -> Result<(), ColumnError> {
        if index >= self.len {
            return Err(ColumnError::InvalidBatch {
                reason: "bitmap index out of bounds",
            });
        }
        let w = &mut self.words[index / 64];
        let bit = 1u64 << (index % 64);
        if value {
            *w |= bit;
        } else {
            *w &= !bit;
        }
        Ok(())
    }

    /// Appends one bit.
    pub fn push(&mut self, value: bool) {
        let idx = self.len;
        self.len += 1;
        let words_len = self.len.div_ceil(64);
        if words_len > self.words.len() {
            self.words.push(0);
        }
        if value {
            let _ = self.set(idx, true);
        }
    }

    /// Number of set bits (`1`).
    pub fn count_ones(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Number of clear bits (`0`).
    pub fn count_zeros(&self) -> usize {
        self.len - self.count_ones()
    }

    /// True if all `len` bits are `1`.
    pub fn all_ones(&self) -> bool {
        if self.len == 0 {
            return true;
        }
        self.count_ones() == self.len
    }

    /// True if all `len` bits are `0`.
    pub fn all_zeros(&self) -> bool {
        if self.len == 0 {
            return true;
        }
        self.count_ones() == 0
    }

    /// Slices `offset..offset + len`.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len) {
            return Err(ColumnError::InvalidBatch {
                reason: "bitmap slice out of bounds",
            });
        }
        let mut res = Self::new_zeroed(len);
        for i in 0..len {
            res.set(i, self.get(offset + i))?;
        }
        Ok(res)
    }

    /// Bitwise AND.
    pub fn and(&self, other: &Self) -> Result<Self, ColumnError> {
        if self.len != other.len {
            return Err(ColumnError::InvalidBatch {
                reason: "bitmap length mismatch for AND",
            });
        }
        let words: Vec<u64> = self
            .words
            .iter()
            .zip(&other.words)
            .map(|(&a, &b)| a & b)
            .collect();
        Self::from_words(words, self.len)
    }

    /// Bitwise OR.
    pub fn or(&self, other: &Self) -> Result<Self, ColumnError> {
        if self.len != other.len {
            return Err(ColumnError::InvalidBatch {
                reason: "bitmap length mismatch for OR",
            });
        }
        let words: Vec<u64> = self
            .words
            .iter()
            .zip(&other.words)
            .map(|(&a, &b)| a | b)
            .collect();
        Self::from_words(words, self.len)
    }

    /// Bitwise NOT.
    pub fn not(&self) -> Self {
        let words: Vec<u64> = self.words.iter().map(|w| !w).collect();
        let mut res = Self {
            words,
            len: self.len,
        };
        clear_trailing_bits(&mut res.words, res.len);
        res
    }

    /// Raw backing words.
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    /// Deterministic logical byte footprint.
    pub fn memory_bytes(&self) -> usize {
        self.words.len() * 8
    }

    /// Iterates indices of all set bits in ascending order.
    pub fn iter_ones(&self) -> impl Iterator<Item = usize> + '_ {
        BitmapIter {
            bitmap: self,
            word_idx: 0,
            bit_idx: 0,
            current_word: self.words.first().copied().unwrap_or(0),
        }
    }
}

struct BitmapIter<'a> {
    bitmap: &'a Bitmap,
    word_idx: usize,
    bit_idx: usize,
    current_word: u64,
}

impl Iterator for BitmapIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.current_word == 0 {
            self.word_idx += 1;
            if self.word_idx >= self.bitmap.words.len() {
                return None;
            }
            self.current_word = self.bitmap.words[self.word_idx];
            self.bit_idx = self.word_idx * 64;
        }
        let tz = self.current_word.trailing_zeros() as usize;
        let pos = self.bit_idx + tz;
        if pos >= self.bitmap.len {
            return None;
        }
        self.current_word &= self.current_word - 1;
        Some(pos)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn trailing_bits_are_always_zero() {
        for len in [0, 1, 63, 64, 65, 127, 128, 129] {
            let ones = Bitmap::new_ones(len);
            assert_eq!(ones.count_ones(), len);
            if len > 0 {
                let last_word_idx = (len - 1) / 64;
                let rem = len % 64;
                if rem != 0 {
                    let expected_mask = (1u64 << rem) - 1;
                    assert_eq!(ones.words[last_word_idx] & !expected_mask, 0);
                }
            }
            let inverted = ones.not();
            assert!(inverted.all_zeros());
            assert_eq!(Bitmap::from_bytes(&ones.to_bytes(), len).unwrap(), ones);
            assert!(Bitmap::from_bytes(&[0u8; 32], len).is_err() || len.div_ceil(8) == 32);
        }
    }

    proptest! {
        #[test]
        fn round_trip_bytes_and_iter_ones(bools in prop::collection::vec(any::<bool>(), 0..=200)) {
            let bm = Bitmap::from_bools(&bools);
            prop_assert_eq!(bm.len(), bools.len());
            let expected: Vec<usize> = bools
                .iter()
                .enumerate()
                .filter_map(|(i, &b)| b.then_some(i))
                .collect();
            let actual: Vec<usize> = bm.iter_ones().collect();
            prop_assert_eq!(actual, expected);
            prop_assert_eq!(bm.count_ones(), bools.iter().filter(|&&b| b).count());
            prop_assert_eq!(&Bitmap::from_bytes(&bm.to_bytes(), bools.len()).unwrap(), &bm);
            prop_assert_eq!(&bm.not().not(), &bm);
            if !bools.is_empty() {
                let mid = bools.len() / 2;
                let sliced = bm.slice(mid, bools.len() - mid).unwrap();
                prop_assert_eq!(sliced, Bitmap::from_bools(&bools[mid..]));
            }
        }
    }
}
