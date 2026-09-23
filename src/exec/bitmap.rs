//! `Bitmap`: a column's validity, one bit per row. A set bit means the row is valid
//! (contract rule 7). `None` in `Column` means "all valid" without allocating one.

/// Row validity, LSB-first within each `u64` word. Bits at or past `len` are always 0, so
/// `count_valid` can sum `count_ones` over every word with no per-word masking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    words: Vec<u64>,
    len: usize,
}

impl Bitmap {
    /// `len` rows, all valid.
    pub fn new_valid(len: usize) -> Self {
        let mut words = vec![u64::MAX; word_count(len)];
        mask_trailing(&mut words, len);
        Bitmap { words, len }
    }

    /// `len` rows, all null.
    pub fn new_null(len: usize) -> Self {
        Bitmap {
            words: vec![0; word_count(len)],
            len,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, i: usize) -> bool {
        assert!(
            i < self.len,
            "bitmap index {i} out of bounds for len {}",
            self.len
        );
        (self.words[i / 64] >> (i % 64)) & 1 == 1
    }

    pub fn set(&mut self, i: usize, valid: bool) {
        assert!(
            i < self.len,
            "bitmap index {i} out of bounds for len {}",
            self.len
        );
        let word = &mut self.words[i / 64];
        if valid {
            *word |= 1 << (i % 64);
        } else {
            *word &= !(1 << (i % 64));
        }
    }

    pub fn push(&mut self, valid: bool) {
        if self.len % 64 == 0 {
            self.words.push(0);
        }
        self.len += 1;
        self.set(self.len - 1, valid);
    }

    pub fn count_valid(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Buffer capacity in bytes: `words.capacity() * 8`, what the §7 byte budget accounts.
    pub fn byte_size(&self) -> usize {
        self.words.capacity() * 8
    }

    /// Raw words, for a stats fast path that skips an all-zero word without a per-row test.
    pub(crate) fn words(&self) -> &[u64] {
        &self.words
    }
}

fn word_count(len: usize) -> usize {
    len.div_ceil(64)
}

/// Clears bits at or past `len` in the last word, so an all-`u64::MAX` fill from
/// `new_valid` never counts padding as valid rows.
fn mask_trailing(words: &mut [u64], len: usize) {
    let rem = len % 64;
    if rem != 0 {
        if let Some(last) = words.last_mut() {
            *last &= (1u64 << rem) - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_valid_is_all_set() {
        let bm = Bitmap::new_valid(70);
        assert_eq!(bm.len(), 70);
        assert_eq!(bm.count_valid(), 70);
        for i in 0..70 {
            assert!(bm.get(i));
        }
    }

    #[test]
    fn new_valid_at_exact_word_boundary() {
        let bm = Bitmap::new_valid(64);
        assert_eq!(bm.count_valid(), 64);
    }

    #[test]
    fn new_null_is_all_clear() {
        let bm = Bitmap::new_null(70);
        assert_eq!(bm.count_valid(), 0);
        for i in 0..70 {
            assert!(!bm.get(i));
        }
    }

    #[test]
    fn set_flips_a_single_bit() {
        let mut bm = Bitmap::new_valid(5);
        bm.set(2, false);
        assert!(!bm.get(2));
        assert!(bm.get(1));
        assert!(bm.get(3));
        assert_eq!(bm.count_valid(), 4);
    }

    #[test]
    fn push_grows_across_word_boundary() {
        let mut bm = Bitmap::new_valid(0);
        for i in 0..130 {
            bm.push(i % 3 != 0);
        }
        assert_eq!(bm.len(), 130);
        for i in 0..130 {
            assert_eq!(bm.get(i), i % 3 != 0);
        }
        assert_eq!(bm.count_valid(), (0..130).filter(|i| i % 3 != 0).count());
    }

    #[test]
    fn count_valid_at_word_boundary() {
        let mut bm = Bitmap::new_valid(64);
        bm.set(63, false);
        bm.push(true);
        assert_eq!(bm.len(), 65);
        assert_eq!(bm.count_valid(), 64);
    }

    #[test]
    #[should_panic]
    fn get_out_of_bounds_panics() {
        let bm = Bitmap::new_valid(3);
        bm.get(3);
    }

    #[test]
    fn byte_size_is_word_capacity_times_eight() {
        let bm = Bitmap::new_valid(1);
        assert_eq!(bm.byte_size(), bm.words.capacity() * 8);
    }
}
