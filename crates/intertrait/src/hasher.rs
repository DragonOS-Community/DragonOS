use core::convert::TryInto;
use core::hash::{BuildHasherDefault, Hasher};
use core::mem::size_of;

/// A simple `Hasher` implementation tuned for performance.
#[derive(Default)]
pub struct FastHasher(u64);

/// A `BuildHasher` for `FastHasher`.
pub type BuildFastHasher = BuildHasherDefault<FastHasher>;

impl Hasher for FastHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut bytes = bytes;
        while bytes.len() > size_of::<u64>() {
            let (u64_bytes, remaining) = bytes.split_at(size_of::<u64>());
            self.0 ^= u64::from_ne_bytes(u64_bytes.try_into().unwrap());
            bytes = remaining
        }
        self.0 ^= bytes
            .iter()
            .fold(0u64, |result, b| (result << 8) | *b as u64);
    }
}
