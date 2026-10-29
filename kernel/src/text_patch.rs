//! Serialized, architecture-safe kernel text modification.
//!
//! Callers queue a complete logical update.  The common layer validates every
//! site before the architecture backend reaches its commit point, so a
//! recoverable error never leaves a partially modified instruction stream.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};

use system_error::SystemError;

/// Stop the machine without unwinding after an executable-text invariant is
/// lost. Continuing could execute corrupted text or free a still-reachable
/// callback, while DragonOS's ordinary panic path may only exit the current
/// kernel thread.
#[cold]
pub(crate) fn fatal_text_invariant() -> ! {
    #[cfg(target_arch = "x86_64")]
    {
        use crate::{
            arch::{interrupt::ipi::send_ipi, CurrentIrqArch},
            exception::{ipi::IpiKind, ipi::IpiTarget, InterruptArch},
        };

        unsafe { CurrentIrqArch::interrupt_disable() };
        send_ipi(IpiKind::StopCpu, IpiTarget::Other);
        loop {
            unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    panic!("executable-text invariant violated");
}

use crate::{
    arch::CurrentIrqArch, exception::InterruptArch, libs::mutex::Mutex, mm::VirtAddr,
    process::ProcessManager,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum TextPatchState {
    Early = 0,
    Live = 1,
    Quiesced = 2,
    Unavailable = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextPatchError {
    Unavailable,
    InvalidContext,
    InvalidTarget,
    InvalidLength,
    Overlap,
    ExpectedMismatch,
    RendezvousTimeout,
    Architecture,
}

impl From<TextPatchError> for SystemError {
    fn from(value: TextPatchError) -> Self {
        match value {
            TextPatchError::Unavailable => SystemError::EOPNOTSUPP_OR_ENOTSUP,
            TextPatchError::InvalidContext => SystemError::EAGAIN_OR_EWOULDBLOCK,
            TextPatchError::InvalidTarget
            | TextPatchError::InvalidLength
            | TextPatchError::Overlap => SystemError::EINVAL,
            TextPatchError::ExpectedMismatch => SystemError::EUCLEAN,
            TextPatchError::RendezvousTimeout => SystemError::ETIMEDOUT,
            TextPatchError::Architecture => SystemError::EIO,
        }
    }
}

pub(crate) struct PreparedTextPatch {
    target: VirtAddr,
    expected: Vec<u8>,
    replacement: Vec<u8>,
}

impl PreparedTextPatch {
    #[inline]
    pub(crate) fn target(&self) -> VirtAddr {
        self.target
    }

    #[inline]
    pub(crate) fn replacement(&self) -> &[u8] {
        &self.replacement
    }

    #[inline]
    fn end(&self) -> usize {
        self.target.data() + self.replacement.len()
    }
}

static TEXT_PATCH_STATE: AtomicU8 = AtomicU8::new(TextPatchState::Early as u8);
static TEXT_PATCH_MUTEX: Mutex<()> = Mutex::new(());

#[inline]
pub(crate) fn state() -> TextPatchState {
    match TEXT_PATCH_STATE.load(Ordering::Acquire) {
        value if value == TextPatchState::Live as u8 => TextPatchState::Live,
        value if value == TextPatchState::Quiesced as u8 => TextPatchState::Quiesced,
        value if value == TextPatchState::Unavailable as u8 => TextPatchState::Unavailable,
        _ => TextPatchState::Early,
    }
}

/// Text patch control operations may sleep and must start in an ordinary
/// preemptible process context. Consumers call this before taking their own
/// sleeping control locks; `TextPatchTransaction::begin` repeats the check at
/// the final backend boundary.
pub(crate) fn validate_control_context() -> Result<(), TextPatchError> {
    if crate::exception::interrupt_context::in_interrupt()
        || !CurrentIrqArch::is_irq_enabled()
        || (ProcessManager::initialized() && ProcessManager::current_pcb().preempt_count() != 0)
    {
        return Err(TextPatchError::InvalidContext);
    }
    Ok(())
}

/// Enable runtime patching after SMP and architecture prerequisites are ready.
pub(crate) fn init_live() -> Result<(), TextPatchError> {
    #[cfg(target_arch = "x86_64")]
    {
        return match crate::arch::text_patch::init() {
            Ok(()) => {
                TEXT_PATCH_STATE.store(TextPatchState::Live as u8, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                TEXT_PATCH_STATE.store(TextPatchState::Unavailable as u8, Ordering::Release);
                Err(error)
            }
        };
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        TEXT_PATCH_STATE.store(TextPatchState::Unavailable as u8, Ordering::Release);
        Err(TextPatchError::Unavailable)
    }
}

/// Permanently close the runtime patch gate during shutdown.
#[allow(dead_code)]
pub(crate) fn quiesce() -> crate::libs::mutex::MutexGuard<'static, ()> {
    let guard = TEXT_PATCH_MUTEX.lock();
    TEXT_PATCH_STATE.store(TextPatchState::Quiesced as u8, Ordering::Release);
    guard
}

/// The sole owning transaction API.  `apply_batch` and static keys both use it;
/// neither layer acquires a second text lock during commit.
pub(crate) struct TextPatchTransaction {
    _guard: crate::libs::mutex::MutexGuard<'static, ()>,
    patches: Vec<PreparedTextPatch>,
}

impl TextPatchTransaction {
    pub(crate) fn begin() -> Result<Self, TextPatchError> {
        validate_control_context()?;

        let guard = TEXT_PATCH_MUTEX.lock();
        if state() != TextPatchState::Live {
            return Err(TextPatchError::Unavailable);
        }
        Ok(Self {
            _guard: guard,
            patches: Vec::new(),
        })
    }

    /// Queue one exact old/new instruction pair.
    ///
    /// # Safety
    ///
    /// `target` must be a static-key site whose complete call graph is
    /// unreachable from NMI/MCE context.  The unsafe key declaration is the
    /// audit boundary for that proof.
    pub(crate) unsafe fn queue(
        &mut self,
        target: *mut core::ffi::c_void,
        expected: &[u8],
        replacement: &[u8],
    ) -> Result<(), TextPatchError> {
        if expected.len() != replacement.len() || expected.is_empty() {
            return Err(TextPatchError::InvalidLength);
        }
        let start = target as usize;
        if start.checked_add(replacement.len()).is_none() {
            return Err(TextPatchError::InvalidTarget);
        }
        self.patches.push(PreparedTextPatch {
            target: VirtAddr::new(start),
            expected: expected.to_vec(),
            replacement: replacement.to_vec(),
        });
        Ok(())
    }

    pub(crate) fn commit(mut self) -> Result<(), TextPatchError> {
        self.patches
            .sort_unstable_by_key(|patch| patch.target.data());
        for pair in self.patches.windows(2) {
            if pair[0].end() > pair[1].target.data() {
                return Err(TextPatchError::Overlap);
            }
        }

        for patch in &self.patches {
            validate_text_range(patch)?;
            let current = unsafe {
                core::slice::from_raw_parts(patch.target.as_ptr::<u8>(), patch.expected.len())
            };
            if current != patch.expected {
                return Err(TextPatchError::ExpectedMismatch);
            }
        }

        if self.patches.is_empty() {
            return Ok(());
        }

        #[cfg(target_arch = "x86_64")]
        return crate::arch::text_patch::commit(&self.patches);

        #[cfg(not(target_arch = "x86_64"))]
        Err(TextPatchError::Unavailable)
    }
}

fn validate_text_range(patch: &PreparedTextPatch) -> Result<(), TextPatchError> {
    extern "C" {
        fn _text();
        fn _etext();
        #[cfg(target_arch = "x86_64")]
        fn __text_no_patch_start();
        #[cfg(target_arch = "x86_64")]
        fn __text_no_patch_end();
    }

    let start = patch.target.data();
    let end = patch.end();
    let text_start = _text as *const () as usize;
    let text_end = _etext as *const () as usize;
    if start < text_start || end > text_end {
        return Err(TextPatchError::InvalidTarget);
    }

    #[cfg(target_arch = "x86_64")]
    {
        let protected_start = __text_no_patch_start as *const () as usize;
        let protected_end = __text_no_patch_end as *const () as usize;
        if start < protected_end && end > protected_start {
            return Err(TextPatchError::InvalidTarget);
        }
    }
    Ok(())
}
