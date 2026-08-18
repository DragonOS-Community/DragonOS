//! DragonOS static-key integration.
//!
//! Static-key declarations in the kernel must use the audited macro from this
//! module.  The upstream default backend is intentionally not used because it
//! performs an unsynchronised `memcpy` into executable text.

use static_keys::{
    code_manipulate::{CodePatchBackend, CodePatchTransaction},
    RawStaticFalseKey,
};

use crate::text_patch::{TextPatchError, TextPatchTransaction};

pub(crate) struct DragonOsTextPatchBackend;

pub(crate) struct DragonOsStaticKeyTransaction(TextPatchTransaction);

unsafe impl CodePatchBackend for DragonOsTextPatchBackend {
    type Error = TextPatchError;
    type Transaction = DragonOsStaticKeyTransaction;

    fn begin() -> Result<Self::Transaction, Self::Error> {
        TextPatchTransaction::begin().map(DragonOsStaticKeyTransaction)
    }
}

unsafe impl CodePatchTransaction for DragonOsStaticKeyTransaction {
    type Error = TextPatchError;

    unsafe fn queue<const L: usize>(
        &mut self,
        addr: *mut core::ffi::c_void,
        expected: &[u8; L],
        replacement: &[u8; L],
    ) -> Result<(), Self::Error> {
        unsafe { self.0.queue(addr, expected, replacement) }
    }

    fn commit(self) -> Result<(), Self::Error> {
        self.0.commit()
    }
}

pub(crate) type MaskableStaticFalseKey = RawStaticFalseKey<DragonOsTextPatchBackend>;

#[inline]
pub(crate) fn enable_maskable_key(
    key: &'static MaskableStaticFalseKey,
) -> Result<(), TextPatchError> {
    // SAFETY: the only constructors are audited declarations below; the
    // backend serializes updates and performs an all-CPU instruction sync.
    match unsafe { key.enable() } {
        Err(TextPatchError::ExpectedMismatch | TextPatchError::Architecture) => {
            panic!("static-key text invariant violated")
        }
        result => result,
    }
}

#[inline]
pub(crate) fn disable_maskable_key(
    key: &'static MaskableStaticFalseKey,
) -> Result<(), TextPatchError> {
    // SAFETY: see `enable_maskable_key`.
    match unsafe { key.disable() } {
        Err(TextPatchError::ExpectedMismatch | TextPatchError::Architecture) => {
            panic!("static-key text invariant violated")
        }
        result => result,
    }
}

/// Declare a static key whose branch site is unreachable from NMI/MCE paths.
///
/// # Safety
///
/// This is an unsafe declaration even though `macro_rules!` cannot require an
/// unsafe block.  Every invocation needs a nearby `SAFETY` justification based
/// on a call-graph audit.  Misclassifying an NMI/MCE-reachable site can expose
/// x86 to a torn five-byte instruction while maskable CPUs are parked.
#[macro_export]
macro_rules! unsafe_define_maskable_static_key_false {
    ($key:ident) => {
        static_keys::define_static_key_false_generic!(
            $key,
            $crate::debug::jump_label::DragonOsTextPatchBackend
        );
    };
}

#[cfg(feature = "static_keys_test")]
mod tests {
    use alloc::{boxed::Box, sync::Arc};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use static_keys::static_branch_unlikely;

    use crate::{
        libs::cpumask::CpuMask,
        process::{
            kthread::{KernelThreadClosure, KernelThreadMechanism},
            ProcessManager,
        },
        sched::completion::Completion,
        smp::{core::smp_get_processor_id, cpu::smp_cpu_manager},
    };

    // SAFETY: this private boot selftest is called only from the initial kernel
    // thread after SMP init and is unreachable from NMI/MCE handlers.
    unsafe_define_maskable_static_key_false!(MY_STATIC_KEY);

    #[inline(always)]
    fn branch_a() -> bool {
        static_branch_unlikely!(MY_STATIC_KEY)
    }

    #[inline(always)]
    fn branch_b() -> bool {
        static_branch_unlikely!(MY_STATIC_KEY)
    }

    pub(super) fn run() -> Result<(), crate::text_patch::TextPatchError> {
        if branch_a() || branch_b() {
            return Err(crate::text_patch::TextPatchError::Architecture);
        }
        super::enable_maskable_key(&MY_STATIC_KEY)?;
        if !branch_a() || !branch_b() {
            return Err(crate::text_patch::TextPatchError::Architecture);
        }
        super::disable_maskable_key(&MY_STATIC_KEY)?;
        if branch_a() || branch_b() {
            return Err(crate::text_patch::TextPatchError::Architecture);
        }
        run_remote_cpu_test()?;
        Ok(())
    }

    fn run_remote_cpu_test() -> Result<(), crate::text_patch::TextPatchError> {
        let current = smp_get_processor_id();
        let Some(remote) = smp_cpu_manager()
            .present_cpus()
            .iter_cpu()
            .find(|cpu| *cpu != current && smp_cpu_manager().is_online_cpu(*cpu))
        else {
            return Ok(());
        };

        let command = Arc::new(AtomicUsize::new(0));
        let acknowledged = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(Completion::new());
        let worker_command = command.clone();
        let worker_acknowledged = acknowledged.clone();
        let worker_observed = observed.clone();
        let worker_ready = ready.clone();
        let closure = KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                worker_ready.complete();
                loop {
                    let value = worker_command.load(Ordering::Acquire);
                    if value == 0 {
                        core::hint::spin_loop();
                        continue;
                    }
                    if value == 4 {
                        return 0;
                    }
                    let taken = branch_a() as usize;
                    worker_observed.store(taken, Ordering::Release);
                    worker_acknowledged.store(value, Ordering::Release);
                    while worker_command.load(Ordering::Acquire) == value {
                        core::hint::spin_loop();
                    }
                }
            }),
            (),
        ));
        let Some(worker) = KernelThreadMechanism::create(closure, "static-key-smp-selftest".into())
        else {
            return Err(crate::text_patch::TextPatchError::Architecture);
        };
        worker
            .sched_info()
            .set_cpus_allowed(CpuMask::from_cpu(remote));
        let test_result = (|| {
            ProcessManager::wakeup(&worker)
                .map_err(|_| crate::text_patch::TextPatchError::Architecture)?;
            ready
                .wait_for_completion()
                .map_err(|_| crate::text_patch::TextPatchError::Architecture)?;

            check_remote_epoch(&command, &acknowledged, &observed, 1, false)?;
            super::enable_maskable_key(&MY_STATIC_KEY)?;
            check_remote_epoch(&command, &acknowledged, &observed, 2, true)?;
            super::disable_maskable_key(&MY_STATIC_KEY)?;
            check_remote_epoch(&command, &acknowledged, &observed, 3, false)
        })();
        let key_cleanup = if MY_STATIC_KEY.is_enabled() {
            super::disable_maskable_key(&MY_STATIC_KEY)
        } else {
            Ok(())
        };
        command.store(4, Ordering::Release);
        let worker_cleanup = KernelThreadMechanism::stop(&worker)
            .map(|_| ())
            .map_err(|_| crate::text_patch::TextPatchError::Architecture);
        test_result.and(key_cleanup).and(worker_cleanup)
    }

    fn check_remote_epoch(
        command: &AtomicUsize,
        acknowledged: &AtomicUsize,
        observed: &AtomicUsize,
        epoch: usize,
        expected: bool,
    ) -> Result<(), crate::text_patch::TextPatchError> {
        command.store(epoch, Ordering::Release);
        while acknowledged.load(Ordering::Acquire) != epoch {
            core::hint::spin_loop();
        }
        if (observed.load(Ordering::Acquire) != 0) != expected {
            return Err(crate::text_patch::TextPatchError::Architecture);
        }
        Ok(())
    }
}

pub fn static_keys_init() {
    // Metadata initialization only; runtime text writes remain gated until
    // `text_patch::init_live()` completes after SMP bring-up.
    static_keys::global_init();
}

pub(crate) fn static_keys_live_selftest() -> Result<(), TextPatchError> {
    #[cfg(feature = "static_keys_test")]
    tests::run()?;
    Ok(())
}
