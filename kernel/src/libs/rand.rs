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
    drop(slot);
    RANDOM_READY_WAIT.wake_all();
    // Registration runs in the initial kernel thread after scheduler setup.
    // Prime even when the first userspace caller only uses GRND_NONBLOCK.
    if let Err(error) = bootstrap_random() {
        log::warn!("secure random bootstrap deferred: {error:?}");
    }
    Ok(())
}

// This adapter owns readiness and synchronization; the pure core owns keys.
static RANDOM_POOL: SpinLock<Option<kernel_rng::Pool>> = SpinLock::new(None);
static RANDOM_READY_WAIT: crate::libs::wait_queue::WaitQueue =
    crate::libs::wait_queue::WaitQueue::default();
pub(crate) static RANDOM_EPITEMS: crate::filesystem::epoll::event_poll::EPollItemList =
    crate::filesystem::epoll::event_poll::EPollItemList::new();

fn with_pool<R>(f: impl FnOnce(&mut kernel_rng::Pool) -> R) -> R {
    let mut guard = RANDOM_POOL.lock_irqsave();
    let pool = guard.get_or_insert_with(|| {
        let mut pool = kernel_rng::Pool::new();
        // Diversify explicitly uncredited early output. This is not an entropy
        // source and can never initialize security-sensitive random requests.
        pool.mix(&rand().to_ne_bytes());
        pool
    });
    f(pool)
}

pub fn random_is_ready() -> bool {
    with_pool(|pool| pool.ready())
}

pub fn random_init_bits() -> u32 {
    with_pool(|pool| pool.init_bits())
}

pub fn mix_random_bytes(input: &[u8]) {
    for chunk in input.chunks(kernel_rng::OUTPUT_CHUNK) {
        with_pool(|pool| pool.mix(chunk));
    }
}

pub fn credit_random_bits(bits: u32) {
    let became_ready = with_pool(|pool| {
        let before = pool.ready();
        pool.credit(bits);
        !before && pool.ready()
    });
    if became_ready {
        RANDOM_READY_WAIT.wake_all();
        use crate::filesystem::epoll::{event_poll::EventPoll, EPollEventType};
        let _ = EventPoll::wakeup_epoll(
            &RANDOM_EPITEMS,
            EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
        );
    }
}

pub fn reseed_random() -> Result<(), SystemError> {
    if with_pool(|pool| pool.reseed()) {
        Ok(())
    } else {
        Err(SystemError::ENODATA)
    }
}

/// Make a bounded bootstrap attempt without holding any RNG/source lock over
/// provider I/O. The provider may sleep and already has a finite request timeout.
fn bootstrap_random() -> Result<(), SystemError> {
    if random_is_ready() {
        return Ok(());
    }
    let source = SECURE_ENTROPY_SOURCE
        .lock_irqsave()
        .as_ref()
        .cloned()
        .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
    let mut seed = zeroize::Zeroizing::new([0u8; 32]);
    source.fill_bytes(&mut seed[..])?;
    mix_random_bytes(&seed[..]);
    credit_random_bits(kernel_rng::READY_BITS);
    Ok(())
}

/// NONBLOCK only observes readiness, never a sleeping entropy device. Blocking
/// callers retry temporary device failure, so a timed-out first request cannot
/// strand the system with every reader waiting for a request nobody submits.
pub fn wait_for_random_ready(nonblocking: bool) -> Result<(), SystemError> {
    if random_is_ready() {
        return Ok(());
    }
    if nonblocking {
        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
    }
    loop {
        if SECURE_ENTROPY_SOURCE.lock_irqsave().is_none() {
            RANDOM_READY_WAIT.wait_event_interruptible(
                || random_is_ready() || SECURE_ENTROPY_SOURCE.lock_irqsave().is_some(),
                None::<fn()>,
            )?;
        }
        match bootstrap_random() {
            Ok(()) => return Ok(()),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                crate::time::sleep::nanosleep(crate::time::PosixTimeSpec::new(0, 10_000_000))?;
            }
            Err(SystemError::ERESTARTSYS) | Err(SystemError::EINTR) => {
                return Err(SystemError::ERESTARTSYS);
            }
            Err(_) => {
                // Device failures are not part of the random userspace ABI.
                // A failed source cannot recover here; wait interruptibly for
                // another trusted injection instead of hot-looping on EIO.
                RANDOM_READY_WAIT.wait_event_interruptible(random_is_ready, None::<fn()>)?;
                return Ok(());
            }
        }
    }
}

/// Output from the shared ChaCha state. Only urandom/GRND_INSECURE may permit
/// uncredited early output; ordinary callers check readiness first.
pub fn fill_random_bytes(output: &mut [u8]) {
    for chunk in output.chunks_mut(kernel_rng::OUTPUT_CHUNK) {
        with_pool(|pool| pool.fill_chunk(chunk));
    }
}

/// Stream bounded output without holding RNG/private-file locks over a user
/// fault. Both getrandom and random devices preserve an already copied prefix.
pub fn read_random_output(
    len: usize,
    insecure: bool,
    nonblocking: bool,
    mut copy: impl FnMut(usize, &[u8]) -> Result<usize, SystemError>,
) -> Result<usize, SystemError> {
    if len == 0 {
        return Ok(0);
    }
    if !insecure {
        wait_for_random_ready(nonblocking)?;
    }
    let mut block = zeroize::Zeroizing::new([0u8; kernel_rng::OUTPUT_CHUNK]);
    let mut completed = 0;
    while completed < len {
        if completed != 0 && crate::process::ProcessManager::current_pcb().has_pending_signal_fast()
        {
            break;
        }
        let count = (len - completed).min(block.len());
        fill_random_bytes(&mut block[..count]);
        match copy(completed, &block[..count]) {
            Ok(copied) if copied <= count => {
                completed += copied;
                if copied != count {
                    return if completed == 0 {
                        Err(SystemError::EFAULT)
                    } else {
                        Ok(completed)
                    };
                }
            }
            Ok(_) => return Err(SystemError::EIO),
            Err(error) => {
                return if completed == 0 {
                    Err(error)
                } else {
                    Ok(completed)
                }
            }
        }
    }
    Ok(completed)
}

/// Preserve the existing fallible kernel API: try bounded bootstrap, but do not
/// turn a key-serial allocation into an unbounded readiness wait.
pub fn secure_random_bytes(output: &mut [u8]) -> Result<(), SystemError> {
    if output.is_empty() {
        return Ok(());
    }
    if let Err(error) = bootstrap_random() {
        output.fill(0);
        return Err(error);
    }
    fill_random_bytes(output);
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
