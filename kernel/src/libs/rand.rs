use crate::arch::rand::rand;
use core::sync::atomic::{AtomicU64, Ordering};

bitflags! {
    pub struct GRandFlags: u8{
        const GRND_NONBLOCK = 0x0001;
        const GRND_RANDOM = 0x0002;
        const GRND_INSECURE = 0x0004;
    }
}

/// Generates an array of random bytes of size `N`.
///
/// This function fills an array of size `N` with random bytes by repeatedly
/// generating random numbers and converting them to little-endian byte arrays.
/// The function ensures that the entire array is filled with random bytes,
/// even if the size of the array is not a multiple of the size of `usize`.
///
/// # Type Parameters
///
/// * `N`: The size of the array to be filled with random bytes.
///
/// # Returns
///
/// An array of size `N` filled with random bytes.
///
/// # Example
///
/// ```rust
/// let random_bytes = rand_bytes::<16>();
/// assert_eq!(random_bytes.len(), 16);
/// ```
pub fn rand_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    let mut remaining = N;
    let mut index = 0;

    while remaining > 0 {
        let random_num = rand();
        let random_bytes = random_num.to_le_bytes();

        let to_copy = core::cmp::min(remaining, size_of::<usize>());
        bytes[index..index + to_copy].copy_from_slice(&random_bytes[..to_copy]);

        index += to_copy;
        remaining -= to_copy;
    }

    bytes
}

// 软件实现的随机数生成器
#[allow(dead_code)]
pub fn soft_rand() -> usize {
    static SEED: AtomicU64 = AtomicU64::new(0xdead_beef_cafe_babe);
    let mut buf = [0u8; size_of::<usize>()];
    for x in buf.iter_mut() {
        let mut current = SEED.load(Ordering::Relaxed);
        loop {
            // Linear congruential step inherited from the existing musl-style
            // fallback. Atomic CAS avoids cross-CPU data races on architectures
            // without a hardware random source.
            let next = current.wrapping_mul(0x5851_f42d_4c95_7f2d);
            match SEED.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => {
                    *x = (next >> 33) as u8;
                    break;
                }
                Err(actual) => current = actual,
            }
        }
    }
    let x: usize = usize::from_ne_bytes(buf);
    return x;
}
