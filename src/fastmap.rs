//! Hash maps for keys the database assigns itself: dictionary ids, tuples and lists of them.
//! They use the Fx hash (one add and multiply per word, as the Rust compiler uses internally)
//! instead of the standard library's SipHash, which costs far more per key.
//!
//! Fx is not resistant to keys chosen to collide, so these maps are only for keys a client
//! cannot pick: ids are handed out in sequence by the dictionary. Maps keyed by term text or
//! other client-supplied strings keep the standard hasher.

use std::hash::{BuildHasherDefault, Hasher};

/// A `HashMap` with the Fx hash (see the module documentation for which keys it suits).
pub type FastMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// A `HashSet` with the Fx hash (see the module documentation for which keys it suits).
pub type FastSet<K> = std::collections::HashSet<K, BuildHasherDefault<FxHasher>>;

const SEED: u64 = 0xf135_7aea_2e62_a9c5;

/// The Fx hasher: each word is added, then the state multiplied by an odd constant.
#[derive(Default, Clone, Copy)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = self.hash.wrapping_add(word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        // slices of integers (a `Vec<Id>` key) arrive here as bytes
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            // the length keeps "a" and "a\0" apart
            self.add(u64::from_le_bytes(last) ^ ((rest.len() as u64) << 59));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(i as u64);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        // the multiply leaves the entropy in the high bits; tables index by the low ones
        self.hash.rotate_left(26)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{BuildHasher, Hash};

    fn hash<T: Hash>(t: &T) -> u64 {
        BuildHasherDefault::<FxHasher>::default().hash_one(t)
    }

    #[test]
    fn spreads_sequential_ids_over_the_low_bits() {
        // tables pick buckets by the low bits: sequential ids must not pile up
        let mut buckets = [0u32; 1024];
        for id in 0u32..100_000 {
            buckets[(hash(&id) & 1023) as usize] += 1;
        }
        let (min, max) = (buckets.iter().min().unwrap(), buckets.iter().max().unwrap());
        assert!(*min > 50 && *max < 150, "{min}..{max}");
    }

    #[test]
    fn keys_that_differ_hash_differently() {
        assert_ne!(hash(&vec![1u32, 2]), hash(&vec![2u32, 1]));
        assert_ne!(hash(&vec![1u32]), hash(&vec![1u32, 0]));
        assert_ne!(hash(&Some(0u32)), hash(&None::<u32>));
        let raw = |b: &[u8]| {
            let mut h = FxHasher::default();
            h.write(b);
            h.finish()
        };
        assert_ne!(raw(&[0]), raw(&[0, 0]), "the tail's length counts");
        let mut m: FastMap<Vec<u32>, usize> = FastMap::default();
        for i in 0..10_000u32 {
            m.insert(vec![i, i / 7], i as usize);
        }
        assert_eq!(m.len(), 10_000);
        assert_eq!(m[&vec![700, 100]], 700);
    }
}
