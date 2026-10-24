//! Allocation-free random-pool state. Device I/O, waiting and ABI policy belong
//! to the kernel adapter; this core never treats uncredited input as entropy.
#![no_std]

use blake2::{
    digest::{KeyInit, Mac},
    Blake2sMac256,
};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use zeroize::Zeroize;

pub const READY_BITS: u32 = 256;
pub const OUTPUT_CHUNK: usize = 256;

#[cfg_attr(test, derive(Clone))]
pub struct Pool {
    input: Blake2sMac256,
    generator: ChaCha20Rng,
    init_bits: u32,
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

impl Pool {
    pub fn new() -> Self {
        // Early output is explicitly uncredited. It must never be exposed by
        // an API promising initialized randomness until credit reaches READY_BITS.
        Self {
            input: <Blake2sMac256 as KeyInit>::new_from_slice(&[0; 32]).unwrap(),
            generator: ChaCha20Rng::from_seed([0; 32]),
            init_bits: 0,
        }
    }

    pub fn ready(&self) -> bool {
        self.init_bits == READY_BITS
    }
    pub fn init_bits(&self) -> u32 {
        self.init_bits
    }

    pub fn mix(&mut self, bytes: &[u8]) {
        Mac::update(&mut self.input, bytes);
        // Before initialization even early readers benefit from newly mixed
        // input, without pretending that it has any credited entropy.
        if !self.ready() {
            self.extract();
        }
    }

    pub fn credit(&mut self, bits: u32) {
        if self.ready() {
            return;
        }
        self.init_bits = self.init_bits.saturating_add(bits).min(READY_BITS);
        if self.ready() {
            self.extract();
        }
    }

    /// Explicit reseeding cannot initialize a generator without trusted credit.
    pub fn reseed(&mut self) -> bool {
        if !self.ready() {
            return false;
        }
        self.extract();
        true
    }

    fn extract(&mut self) {
        let old = core::mem::replace(
            &mut self.input,
            <Blake2sMac256 as KeyInit>::new_from_slice(&[0; 32]).unwrap(),
        );
        let mut seed: [u8; 32] = old.finalize().into_bytes().into();
        let derive = |domain: &[u8]| -> [u8; 32] {
            let mut mac = <Blake2sMac256 as KeyInit>::new_from_slice(&seed).unwrap();
            Mac::update(&mut mac, domain);
            mac.finalize().into_bytes().into()
        };
        let mut next_pool = derive(b"DragonOS input-pool key");
        let mut next_rng = derive(b"DragonOS random-output key");
        self.input = <Blake2sMac256 as KeyInit>::new_from_slice(&next_pool).unwrap();
        self.generator = ChaCha20Rng::from_seed(next_rng);
        seed.zeroize();
        next_pool.zeroize();
        next_rng.zeroize();
    }

    /// Bounded output with a key ratchet. Callers choose whether uncredited
    /// early output is permitted; ready is never changed by generating bytes.
    pub fn fill_chunk(&mut self, output: &mut [u8]) {
        assert!(output.len() <= OUTPUT_CHUNK);
        if output.is_empty() {
            return;
        }
        self.generator.fill_bytes(output);
        let mut next_key = [0; 32];
        self.generator.fill_bytes(&mut next_key);
        self.generator = ChaCha20Rng::from_seed(next_key);
        next_key.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> Pool {
        let mut pool = Pool::new();
        pool.mix(&[42; 32]);
        pool.credit(READY_BITS);
        pool
    }

    #[test]
    fn mixing_is_not_credit_and_reseed_needs_ready() {
        let mut pool = Pool::new();
        pool.mix(&[19; 512]);
        assert_eq!(pool.init_bits(), 0);
        assert!(!pool.reseed());
        pool.credit(255);
        assert!(!pool.ready());
        pool.credit(u32::MAX);
        assert!(pool.ready());
        assert_eq!(pool.init_bits(), READY_BITS);
    }

    #[test]
    fn restored_snapshots_diverge_after_independent_injection() {
        let snapshot = seeded();
        let mut left = snapshot.clone();
        let mut right = snapshot.clone();
        left.mix(&[1; 512]);
        right.mix(&[2; 512]);
        assert!(left.reseed());
        assert!(right.reseed());
        let mut a = [0; OUTPUT_CHUNK];
        let mut b = [0; OUTPUT_CHUNK];
        left.fill_chunk(&mut a);
        right.fill_chunk(&mut b);
        assert_ne!(a, b);
    }

    #[test]
    fn reseed_and_output_both_advance_state() {
        let mut left = seeded();
        let mut right = left.clone();
        right.reseed();
        let mut a = [0; OUTPUT_CHUNK];
        let mut b = [0; OUTPUT_CHUNK];
        left.fill_chunk(&mut a);
        right.fill_chunk(&mut b);
        assert_ne!(a, b);
        left.fill_chunk(&mut b);
        assert_ne!(a, b);
        assert_eq!(left.init_bits(), READY_BITS);
    }

    #[test]
    fn early_output_does_not_initialize_and_zero_output_preserves_state() {
        let mut left = Pool::new();
        let mut right = left.clone();
        left.fill_chunk(&mut []);
        let mut a = [0; 33];
        let mut b = [0; 33];
        left.fill_chunk(&mut a);
        right.fill_chunk(&mut b);
        assert_eq!(a, b);
        assert!(!left.ready());
        left.mix(b"uncredited boot input");
        right.fill_chunk(&mut a);
        left.fill_chunk(&mut b);
        assert_ne!(a, b);
        assert!(!left.ready());
    }
}
