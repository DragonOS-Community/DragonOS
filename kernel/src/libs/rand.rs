use crate::arch::rand::rand;
use crate::libs::spinlock::SpinLock;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use system_error::SystemError;

/// A provider of entropy suitable for security-sensitive kernel uses.
///
/// This is separate from the legacy `rand_bytes`, whose clock/LCG sources
/// are not suitable for key serial allocation.
pub trait SecureEntropySource: Send + Sync {
    fn fill_bytes(&self, output: &mut [u8]) -> Result<(), SystemError>;
}

static SECURE_ENTROPY_SOURCE: SpinLock<Option<Arc<dyn SecureEntropySource>>> = SpinLock::new(None);

pub fn register_secure_entropy_source(
    source: Arc<dyn SecureEntropySource>,
) -> Result<(), SystemError> {
    let mut slot = SECURE_ENTROPY_SOURCE.lock();
    if slot.is_some() {
        return Err(SystemError::EBUSY);
    }
    *slot = Some(source);
    Ok(())
}

/// Fill a kernel buffer without falling back to the legacy weak generator.
pub fn secure_random_bytes(output: &mut [u8]) -> Result<(), SystemError> {
    if output.is_empty() {
        return Ok(());
    }
    let source = SECURE_ENTROPY_SOURCE
        .lock()
        .as_ref()
        .cloned()
        .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
    if let Err(error) = source.fill_bytes(output) {
        output.fill(0);
        return Err(error);
    }
    Ok(())
}

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
