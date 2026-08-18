//! x86-64 runtime text patching.
//!
//! All maskable remote CPUs are parked before the first byte of a five-byte
//! jump instruction is changed. Writes use two reserved RW+NX, non-global
//! fixmap slots; the executable mapping remains read-only.

use alloc::vec::Vec;
use core::{
    arch::x86_64::{__cpuid, _rdtsc},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use crate::arch::MMArch;
use crate::{
    arch::{
        driver::apic::{CurrentApic, LocalAPIC},
        interrupt::ipi::{send_ipi, IPI_NUM_TEXT_PATCH},
        mm::X86_64MMArch,
        CurrentIrqArch,
    },
    exception::{
        ipi::{IpiKind, IpiTarget},
        HardwareIrqNumber, InterruptArch,
    },
    mm::{
        kernel_mapper::KernelMapper,
        page::{EntryFlags, PageEntry, PageTable},
        MemoryManagementArch, PageTableKind, PhysAddr, VirtAddr,
    },
    process::preempt::PreemptGuard,
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, ProcessorId},
    },
    text_patch::{PreparedTextPatch, TextPatchError},
};

const STATE_REQUESTED: u64 = 1;
const STATE_PARKED: u64 = 2;
const STATE_SYNCED: u64 = 3;
const STATE_RELEASED: u64 = 4;
const STATE_CANCELLED: u64 = 5;

const PHASE_PARK: u64 = 1;
const PHASE_SYNC: u64 = 2;
const PHASE_RELEASE: u64 = 3;
const PHASE_CANCEL: u64 = 4;

const STATE_BITS: u32 = 3;
const TSC_PARK_TIMEOUT_CYCLES: u64 = 2_000_000_000;
const PARK_TIMEOUT_SPINS: usize = 100_000_000;

static GENERATION: AtomicU64 = AtomicU64::new(0);
static PHASE: AtomicU64 = AtomicU64::new(0);
static MAILBOX: [AtomicU64; crate::mm::percpu::PerCpu::MAX_CPU_NUM as usize] =
    [const { AtomicU64::new(0) }; crate::mm::percpu::PerCpu::MAX_CPU_NUM as usize];
static ALIAS_PTE: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

#[inline]
const fn tagged(generation: u64, value: u64) -> u64 {
    (generation << STATE_BITS) | value
}

#[inline]
fn tag_generation(value: u64) -> u64 {
    value >> STATE_BITS
}

#[inline]
fn tag_value(value: u64) -> u64 {
    value & ((1 << STATE_BITS) - 1)
}

#[inline]
fn serialize_core() {
    // SAFETY: CPUID is available in long mode and is the architectural serializing
    // instruction Linux uses for `sync_core()` on ordinary x86 CPUs.
    __cpuid(0);
}

#[inline]
fn alias(index: usize) -> VirtAddr {
    debug_assert!(index < 2);
    MMArch::FIXMAP_END_VADDR - ((2 - index) * MMArch::PAGE_SIZE)
}

fn alias_flags() -> EntryFlags<MMArch> {
    EntryFlags::new()
        .set_user(false)
        .set_write(true)
        .set_execute(false)
        .set_page_global(false)
}

/// Prepare the two permanent page-table paths and prove the executable mapping
/// is RX before making the backend visible to consumers.
pub(crate) fn init() -> Result<(), TextPatchError> {
    let has_tsc = raw_cpuid::CpuId::new()
        .get_feature_info()
        .is_some_and(|features| features.has_tsc());
    if X86_64MMArch::is_xd_reserved() || !has_tsc {
        return Err(TextPatchError::Unavailable);
    }

    extern "C" {
        fn _text();
    }
    let text_page = VirtAddr::new((_text as *const () as usize) & MMArch::PAGE_MASK);
    let _preempt = PreemptGuard::new();
    let mut mapper = KernelMapper::lock();
    let mapper_mut = mapper.as_mut().ok_or(TextPatchError::Architecture)?;
    let (text_phys, text_flags) = mapper_mut
        .translate(text_page)
        .ok_or(TextPatchError::Architecture)?;
    if text_flags.has_write() || !text_flags.has_execute() {
        return Err(TextPatchError::Unavailable);
    }

    for (slot, alias_pte) in ALIAS_PTE.iter().enumerate() {
        if mapper_mut.translate(alias(slot)).is_some() {
            return Err(TextPatchError::Architecture);
        }
        let flush = unsafe { mapper_mut.map_phys(alias(slot), text_phys, alias_flags()) }
            .ok_or(TextPatchError::Architecture)?;
        flush.flush();
        let (_, flags, flush) = unsafe { mapper_mut.unmap_phys_preserve_tables(alias(slot)) }
            .ok_or(TextPatchError::Architecture)?;
        if !flags.has_write() || flags.has_execute() {
            return Err(TextPatchError::Architecture);
        }
        flush.flush();
        let pte = alias_leaf_pte(alias(slot)).ok_or(TextPatchError::Architecture)?;
        if unsafe { MMArch::read::<usize>(pte) } != 0 {
            return Err(TextPatchError::Architecture);
        }
        alias_pte.store(pte.data(), Ordering::Release);
    }
    drop(mapper);
    drop(_preempt);

    // The no-write probe also proves every Online AP can receive the dedicated
    // IPI and execute the generation protocol before the public state is Live.
    rendezvous_probe()
}

struct PhysicalPatch<'a> {
    patch: &'a PreparedTextPatch,
    first_page: PhysAddr,
    second_page: Option<PhysAddr>,
}

pub(crate) fn commit(patches: &[PreparedTextPatch]) -> Result<(), TextPatchError> {
    let mut physical = Vec::with_capacity(patches.len());
    let _preempt = PreemptGuard::new();
    let targets = online_remote_cpus();
    {
        let mut mapper = KernelMapper::lock();
        let mapper_mut = mapper.as_mut().ok_or(TextPatchError::Architecture)?;
        for patch in patches {
            if patch.replacement().len() != 5 {
                return Err(TextPatchError::InvalidLength);
            }
            let start = patch.target().data();
            let first_va = VirtAddr::new(start & MMArch::PAGE_MASK);
            let (first_page, first_flags) = mapper_mut
                .translate(first_va)
                .ok_or(TextPatchError::InvalidTarget)?;
            validate_text_page_flags(first_flags)?;
            let crosses_page =
                (start & MMArch::PAGE_OFFSET_MASK) + patch.replacement().len() > MMArch::PAGE_SIZE;
            let second_page = if crosses_page {
                let (page, flags) = mapper_mut
                    .translate(first_va + MMArch::PAGE_SIZE)
                    .ok_or(TextPatchError::InvalidTarget)?;
                validate_text_page_flags(flags)?;
                Some(page)
            } else {
                None
            };
            physical.push(PhysicalPatch {
                patch,
                first_page,
                second_page,
            });
        }
    }
    if !alias_slots_empty() {
        return Err(TextPatchError::Architecture);
    }

    let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    let generation = begin_rendezvous(&targets)?;

    // Commit point: no recoverable operation follows. Parent page tables and
    // physical translations were prepared while remote CPUs were still running.
    for item in &physical {
        unsafe { write_via_alias(item) };
    }
    serialize_core();
    finish_rendezvous(generation, &targets);
    drop(irq_guard);
    Ok(())
}

fn rendezvous_probe() -> Result<(), TextPatchError> {
    let _preempt = PreemptGuard::new();
    let targets = online_remote_cpus();
    let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    let generation = begin_rendezvous(&targets)?;
    serialize_core();
    finish_rendezvous(generation, &targets);
    drop(irq_guard);
    Ok(())
}

fn online_remote_cpus() -> Vec<ProcessorId> {
    let current = smp_get_processor_id();
    smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .filter(|cpu| *cpu != current && smp_cpu_manager().is_online_cpu(*cpu))
        .collect()
}

fn begin_rendezvous(targets: &[ProcessorId]) -> Result<u64, TextPatchError> {
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    PHASE.store(tagged(generation, PHASE_PARK), Ordering::Release);
    for cpu in targets {
        MAILBOX[cpu.data() as usize].store(tagged(generation, STATE_REQUESTED), Ordering::Release);
    }
    for cpu in targets {
        send_ipi(
            IpiKind::SpecVector(HardwareIrqNumber::new(IPI_NUM_TEXT_PATCH.data())),
            IpiTarget::Specified(*cpu),
        );
    }

    let start = unsafe { _rdtsc() };
    let mut spins = 0usize;
    loop {
        if targets.iter().all(|cpu| {
            MAILBOX[cpu.data() as usize].load(Ordering::Acquire) == tagged(generation, STATE_PARKED)
        }) {
            return Ok(generation);
        }
        spins += 1;
        if spins >= PARK_TIMEOUT_SPINS
            || unsafe { _rdtsc() }.wrapping_sub(start) > TSC_PARK_TIMEOUT_CYCLES
        {
            abort_rendezvous(generation, targets);
            return Err(TextPatchError::RendezvousTimeout);
        }
        core::hint::spin_loop();
    }
}

fn abort_rendezvous(generation: u64, targets: &[ProcessorId]) {
    PHASE.store(tagged(generation, PHASE_CANCEL), Ordering::Release);
    for cpu in targets {
        let mailbox = &MAILBOX[cpu.data() as usize];
        let _ = mailbox.compare_exchange(
            tagged(generation, STATE_REQUESTED),
            tagged(generation, STATE_CANCELLED),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
    while !targets.iter().all(|cpu| {
        matches!(
            tag_value(MAILBOX[cpu.data() as usize].load(Ordering::Acquire)),
            STATE_CANCELLED | STATE_RELEASED
        )
    }) {
        core::hint::spin_loop();
    }
}

fn finish_rendezvous(generation: u64, targets: &[ProcessorId]) {
    PHASE.store(tagged(generation, PHASE_SYNC), Ordering::Release);
    while !targets.iter().all(|cpu| {
        MAILBOX[cpu.data() as usize].load(Ordering::Acquire) == tagged(generation, STATE_SYNCED)
    }) {
        core::hint::spin_loop();
    }
    PHASE.store(tagged(generation, PHASE_RELEASE), Ordering::Release);
    while !targets.iter().all(|cpu| {
        MAILBOX[cpu.data() as usize].load(Ordering::Acquire) == tagged(generation, STATE_RELEASED)
    }) {
        core::hint::spin_loop();
    }
}

unsafe fn write_via_alias(item: &PhysicalPatch<'_>) {
    unsafe { set_alias_pte(0, PageEntry::new(item.first_page, alias_flags())) };
    if let Some(second) = item.second_page {
        unsafe { set_alias_pte(1, PageEntry::new(second, alias_flags())) };
    }

    let offset = item.patch.target().data() & MMArch::PAGE_OFFSET_MASK;
    let destination = (alias(0).data() + offset) as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(
            item.patch.replacement().as_ptr(),
            destination,
            item.patch.replacement().len(),
        );
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    let written = unsafe {
        core::slice::from_raw_parts(destination.cast_const(), item.patch.replacement().len())
    };
    if written != item.patch.replacement() {
        fatal_after_commit();
    }

    unsafe { clear_alias_pte(0) };
    if item.second_page.is_some() {
        unsafe { clear_alias_pte(1) };
    }
}

fn validate_text_page_flags(flags: EntryFlags<MMArch>) -> Result<(), TextPatchError> {
    if flags.has_write() || !flags.has_execute() {
        return Err(TextPatchError::Architecture);
    }
    Ok(())
}

fn alias_leaf_pte(virt: VirtAddr) -> Option<VirtAddr> {
    let mut table = unsafe { PageTable::<MMArch>::top_level_table(PageTableKind::Kernel) };
    while table.level() > 0 {
        let index = table.index_of(virt)?;
        let entry = unsafe { table.entry(index)? };
        if !entry.present() || MMArch::entry_is_leaf(table.level(), entry.flags().data()) {
            return None;
        }
        table = unsafe { table.next_level_table(index)? };
    }
    unsafe { table.entry_virt(table.index_of(virt)?) }
}

fn alias_pte(index: usize) -> VirtAddr {
    let address = ALIAS_PTE[index].load(Ordering::Acquire);
    if address == 0 {
        fatal_after_commit();
    }
    VirtAddr::new(address)
}

fn alias_slots_empty() -> bool {
    (0..2).all(|index| {
        let address = ALIAS_PTE[index].load(Ordering::Acquire);
        address != 0 && unsafe { MMArch::read::<usize>(VirtAddr::new(address)) } == 0
    })
}

unsafe fn set_alias_pte(index: usize, entry: PageEntry<MMArch>) {
    let pte = alias_pte(index);
    if unsafe { MMArch::read::<usize>(pte) } != 0 {
        fatal_after_commit();
    }
    unsafe { MMArch::write::<usize>(pte, entry.data()) };
    core::sync::atomic::fence(Ordering::SeqCst);
    unsafe { MMArch::invalidate_page(alias(index)) };
}

unsafe fn clear_alias_pte(index: usize) {
    let pte = alias_pte(index);
    let entry = PageEntry::<MMArch>::from_usize(unsafe { MMArch::read::<usize>(pte) });
    let flags = entry.flags();
    if entry.address().is_err() || !flags.has_write() || flags.has_execute() {
        fatal_after_commit();
    }
    unsafe { MMArch::write::<usize>(pte, 0) };
    core::sync::atomic::fence(Ordering::SeqCst);
    unsafe { MMArch::invalidate_page(alias(index)) };
    if unsafe { MMArch::read::<usize>(pte) } != 0 {
        fatal_after_commit();
    }
}

#[cold]
fn fatal_after_commit() -> ! {
    loop {
        unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
    }
}

/// Dedicated IPI path. It is deliberately lock-free and allocation-free.
#[unsafe(link_section = ".text.no_patch")]
pub(crate) fn ipi_handler() {
    CurrentApic.send_eoi();
    let cpu = smp_get_processor_id();
    let mailbox = &MAILBOX[cpu.data() as usize];
    let requested = mailbox.load(Ordering::Acquire);
    if tag_value(requested) != STATE_REQUESTED {
        return;
    }
    let generation = tag_generation(requested);
    if mailbox
        .compare_exchange(
            requested,
            tagged(generation, STATE_PARKED),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }

    loop {
        let phase = PHASE.load(Ordering::Acquire);
        if tag_generation(phase) != generation {
            mailbox.store(tagged(generation, STATE_CANCELLED), Ordering::Release);
            return;
        }
        match tag_value(phase) {
            PHASE_PARK => core::hint::spin_loop(),
            PHASE_CANCEL => {
                mailbox.store(tagged(generation, STATE_CANCELLED), Ordering::Release);
                return;
            }
            PHASE_SYNC => {
                serialize_core();
                mailbox.store(tagged(generation, STATE_SYNCED), Ordering::Release);
                break;
            }
            _ => core::hint::spin_loop(),
        }
    }

    loop {
        let phase = PHASE.load(Ordering::Acquire);
        if tag_generation(phase) == generation && tag_value(phase) == PHASE_RELEASE {
            mailbox.store(tagged(generation, STATE_RELEASED), Ordering::Release);
            return;
        }
        core::hint::spin_loop();
    }
}
