//! KVM's per-vCPU cumulative host scheduling delay, separate from PVclock.
//!
//! The hypervisor adds to the value already in guest RAM. Both this storage
//! and its consumers' cursors survive transparent RAM/MSR snapshot restore.
use core::{
    cell::UnsafeCell,
    hint::spin_loop,
    mem::{align_of, offset_of, size_of},
    ptr::{addr_of, read_volatile},
    sync::atomic::{fence, AtomicBool, Ordering},
};
use system_error::SystemError;

use crate::{
    arch::{CurrentIrqArch, MMArch},
    exception::InterruptArch,
    mm::{percpu::PerCpu, MemoryManagementArch, VirtAddr},
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
};

use super::{kvm_para_has_feature, KVM_FEATURE_STEAL_TIME, MSR_KVM_STEAL_TIME};

/// Linux KVM ABI: Documentation/virt/kvm/x86/msr.rst, MSR_KVM_STEAL_TIME.
#[repr(C, align(64))]
struct StealTime {
    steal: u64,
    version: u32,
    flags: u32,
    preempted: u8,
    u8_pad: [u8; 3],
    pad: [u32; 11],
}

impl StealTime {
    const fn new() -> Self {
        Self {
            steal: 0,
            version: 0,
            flags: 0,
            preempted: 0,
            u8_pad: [0; 3],
            pad: [0; 11],
        }
    }
}

#[repr(transparent)]
struct SharedStealTime(UnsafeCell<StealTime>);

// SAFETY: guest code never writes a published record. KVM is the external
// writer, using its version protocol; readers use raw volatile accesses and
// load barriers without constructing references into the shared ABI record.
// Storage is permanent, so a remote CPU cannot outlive a registered record.
unsafe impl Sync for SharedStealTime {}

static RECORDS: [SharedStealTime; PerCpu::MAX_CPU_NUM as usize] =
    [const { SharedStealTime(UnsafeCell::new(StealTime::new())) }; PerCpu::MAX_CPU_NUM as usize];
// Guest-only publication metadata must not share the hypervisor's ABI record.
static REGISTERED: [AtomicBool; PerCpu::MAX_CPU_NUM as usize] =
    [const { AtomicBool::new(false) }; PerCpu::MAX_CPU_NUM as usize];

const _: () = {
    assert!(size_of::<StealTime>() == 64);
    assert!(align_of::<StealTime>() == 64);
    assert!(size_of::<SharedStealTime>() == 64);
    assert!(align_of::<SharedStealTime>() == 64);
    assert!(offset_of!(StealTime, steal) == 0);
    assert!(offset_of!(StealTime, version) == 8);
    assert!(offset_of!(StealTime, flags) == 12);
    assert!(offset_of!(StealTime, preempted) == 16);
    assert!(offset_of!(StealTime, pad) == 20);
};

/// Register on the owning CPU, before its scheduler is enabled.
/// Interrupts must remain disabled throughout registration and publication.
pub(crate) fn init_current_cpu() -> Result<(), SystemError> {
    if !kvm_para_has_feature(KVM_FEATURE_STEAL_TIME) {
        return Ok(());
    }
    assert!(!CurrentIrqArch::is_irq_enabled());
    let index = smp_get_processor_id().data() as usize;
    let record = RECORDS.get(index).ok_or(SystemError::ENODEV)?;
    let registered = &REGISTERED[index];
    let address = VirtAddr::new(record.0.get() as usize);
    let physical = unsafe { MMArch::virt_2_phys(address) }.ok_or(SystemError::EFAULT)?;
    if !physical.data().is_multiple_of(align_of::<StealTime>()) {
        return Err(SystemError::EINVAL);
    }
    // A 64-byte-aligned, 64-byte record cannot cross an x86 base page.
    // KVM may retain this GPA indefinitely. Never clear or free the record.
    // Reinstall the MSR on CPU bringup: INIT may clear the host-side MSR even
    // though the guest's registration metadata survived in RAM.
    unsafe { x86::msr::wrmsr(MSR_KVM_STEAL_TIME, physical.data() as u64 | 1) };
    registered.store(true, Ordering::Release);
    Ok(())
}

/// Read the specified CPU, including a remote rq's CPU, without a guest lock.
/// None means registration is unavailable, never a synthetic zero sample.
pub(crate) fn read(cpu: ProcessorId) -> Option<u64> {
    let index = cpu.data() as usize;
    if !REGISTERED.get(index)?.load(Ordering::Acquire) {
        return None;
    }
    let source = RECORDS[index].0.get();
    loop {
        // SAFETY: the published record has static lifetime and is naturally
        // aligned; KVM writes it using the version protocol checked below.
        if let Some(value) = unsafe { read_once(source) } {
            return Some(value);
        }
        spin_loop();
    }
}

/// Terminal CPU teardown only; do not clear the shared cumulative value.
pub(crate) fn shutdown_current_cpu() {
    let registered = &REGISTERED[smp_get_processor_id().data() as usize];
    if registered.load(Ordering::Acquire) {
        unsafe { x86::msr::wrmsr(MSR_KVM_STEAL_TIME, 0) };
        registered.store(false, Ordering::Release);
    }
}

/// One nonblocking attempt; private so selftests can reject odd versions
/// without creating an actual concurrent writer or touching registered RAM.
unsafe fn read_once(source: *const StealTime) -> Option<u64> {
    let version = read_volatile(addr_of!((*source).version));
    if version & 1 != 0 {
        return None;
    }
    fence(Ordering::Acquire);
    let steal = read_volatile(addr_of!((*source).steal));
    fence(Ordering::Acquire);
    (version == read_volatile(addr_of!((*source).version))).then_some(steal)
}

pub(crate) fn run_selftests() -> Result<(), &'static str> {
    // All writes below target a private, unregistered stack fixture.
    let mut fixture = StealTime::new();
    if unsafe { read_once(addr_of!(fixture)) } != Some(0) {
        return Err("KVM steal reader lost initial zero");
    }
    fixture.version = 1;
    fixture.steal = 123;
    if unsafe { read_once(addr_of!(fixture)) }.is_some() {
        return Err("KVM steal reader accepted an in-progress version");
    }
    fixture.version = 2;
    if unsafe { read_once(addr_of!(fixture)) } != Some(123) {
        return Err("KVM steal reader lost a completed cumulative value");
    }
    fixture.version = 4;
    fixture.steal = u64::MAX;
    if unsafe { read_once(addr_of!(fixture)) } != Some(u64::MAX) {
        return Err("KVM steal reader truncated the cumulative value");
    }
    Ok(())
}
