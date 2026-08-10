use alloc::string::ToString;
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use log::{info, warn};
use system_error::SystemError;
use x86::msr::wrmsr;

use crate::{
    arch::{
        kvm_para,
        pvclock::{self, PvclockVcpuTimeInfo, PvclockVsyscallTimeInfo, PVCLOCK_TSC_STABLE_BIT},
        MMArch,
    },
    libs::spinlock::SpinLock,
    mm::{
        allocator::page_frame::FrameAllocator,
        percpu::{PerCpu, PerCpuVar},
        MemoryManagementArch, PhysAddr, VirtAddr,
    },
    smp::core::smp_get_processor_id,
    smp::cpu::{smp_cpu_manager, ProcessorId},
    time::{
        clocksource::{
            Clocksource, ClocksourceData, ClocksourceFlags, ClocksourceMask, ClocksourceUpdate,
            CycleNum,
        },
        NSEC_PER_SEC,
    },
};

use crate::arch::driver::tsc::TSCManager;
use crate::process::preempt::PreemptGuard;
use alloc::sync::Weak;

#[derive(Clone, Copy, Debug)]
struct KvmClockPage {
    phys: PhysAddr,
    virt: VirtAddr,
}

impl KvmClockPage {
    const fn empty() -> Self {
        Self {
            phys: PhysAddr::new(0),
            virt: VirtAddr::new(0),
        }
    }

    fn is_valid(&self) -> bool {
        self.phys.data() != 0 && self.virt.data() != 0
    }
}

static mut KVM_CLOCK_PAGES: Option<PerCpuVar<KvmClockPage>> = None;
static MSR_KVM_SYSTEM_TIME: AtomicU32 = AtomicU32::new(0);
static KVM_CLOCK_READY: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub struct KvmClock(SpinLock<InnerKvmClock>);

#[derive(Debug)]
struct InnerKvmClock {
    data: ClocksourceData,
    self_ref: Weak<KvmClock>,
}

impl KvmClock {
    pub fn new() -> Arc<Self> {
        let data = ClocksourceData {
            name: "kvm-clock".to_string(),
            rating: 400,
            mask: ClocksourceMask::new(u64::MAX),
            mult: 0,
            shift: 0,
            max_cycles: Default::default(),
            max_idle_ns: Default::default(),
            flags: ClocksourceFlags::CLOCK_SOURCE_IS_CONTINUOUS,
            watchdog_last: CycleNum::new(0),
            cs_last: CycleNum::new(0),
            uncertainty_margin: 0,
            maxadj: 0,
        };
        let kvm = Arc::new(KvmClock(SpinLock::new(InnerKvmClock {
            data,
            self_ref: Default::default(),
        })));
        kvm.0.lock().self_ref = Arc::downgrade(&kvm);
        kvm
    }
}

impl Clocksource for KvmClock {
    fn read(&self) -> CycleNum {
        let now = kvm_clock_read().expect("active kvm-clock CPU has no pvclock page");
        CycleNum::new(now)
    }

    fn enable(&self) -> Result<i32, SystemError> {
        Ok(0)
    }

    fn clocksource_data(&self) -> ClocksourceData {
        self.0.lock_irqsave().data.clone()
    }

    fn clocksource(&self) -> Arc<dyn Clocksource> {
        self.0.lock_irqsave().self_ref.upgrade().unwrap()
    }

    fn update_clocksource_data(&self, update: ClocksourceUpdate) -> Result<(), SystemError> {
        update.apply(&mut self.0.lock_irqsave().data);
        Ok(())
    }
}

fn kvm_clock_pages() -> &'static PerCpuVar<KvmClockPage> {
    unsafe { KVM_CLOCK_PAGES.as_ref().unwrap() }
}

fn ensure_kvm_clock_pages() {
    unsafe {
        if KVM_CLOCK_PAGES.is_some() {
            return;
        }
    }

    let mut pages = Vec::with_capacity(PerCpu::MAX_CPU_NUM as usize);
    pages.resize_with(PerCpu::MAX_CPU_NUM as usize, KvmClockPage::empty);
    unsafe {
        KVM_CLOCK_PAGES = Some(PerCpuVar::new(pages).unwrap());
    }
}

trait ClockPageAllocator {
    fn allocate_frame(&mut self) -> Option<PhysAddr>;
    fn map_frame(&mut self, phys: PhysAddr) -> Option<VirtAddr>;
    fn clear_page(&mut self, virt: VirtAddr);
    fn release_frame(&mut self, phys: PhysAddr);
}

struct SystemClockPageAllocator;

impl ClockPageAllocator for SystemClockPageAllocator {
    fn allocate_frame(&mut self) -> Option<PhysAddr> {
        unsafe { crate::arch::mm::LockedFrameAllocator.allocate_one() }
    }

    fn map_frame(&mut self, phys: PhysAddr) -> Option<VirtAddr> {
        unsafe { MMArch::phys_2_virt(phys) }
    }

    fn clear_page(&mut self, virt: VirtAddr) {
        unsafe { MMArch::write_bytes(virt, 0, MMArch::PAGE_SIZE) };
    }

    fn release_frame(&mut self, phys: PhysAddr) {
        unsafe { crate::arch::mm::LockedFrameAllocator.free_one(phys) };
    }
}

fn alloc_clock_page_with<A: ClockPageAllocator>(
    allocator: &mut A,
) -> Result<KvmClockPage, SystemError> {
    let phys = allocator.allocate_frame().ok_or(SystemError::ENOMEM)?;
    let Some(virt) = allocator.map_frame(phys) else {
        allocator.release_frame(phys);
        return Err(SystemError::EINVAL);
    };
    allocator.clear_page(virt);
    Ok(KvmClockPage { phys, virt })
}

fn release_clock_page_with<A: ClockPageAllocator>(allocator: &mut A, page: KvmClockPage) {
    allocator.release_frame(page.phys);
}

fn allocate_missing_pages<A: ClockPageAllocator>(
    missing_cpus: &[ProcessorId],
    allocator: &mut A,
) -> Result<Vec<(ProcessorId, KvmClockPage)>, SystemError> {
    let mut staged = Vec::with_capacity(missing_cpus.len());
    for &cpu_id in missing_cpus {
        match alloc_clock_page_with(allocator) {
            Ok(page) => staged.push((cpu_id, page)),
            Err(error) => {
                for (_, page) in staged.drain(..) {
                    release_clock_page_with(allocator, page);
                }
                return Err(error);
            }
        }
    }
    Ok(staged)
}

fn alloc_clock_page() -> Result<KvmClockPage, SystemError> {
    alloc_clock_page_with(&mut SystemClockPageAllocator)
}

fn ensure_clock_page_for_cpu() -> Result<(KvmClockPage, bool), SystemError> {
    ensure_kvm_clock_pages();
    let cpu_id = smp_get_processor_id();
    let pages = kvm_clock_pages();

    let page = unsafe { pages.force_get_mut(cpu_id) };
    if page.is_valid() {
        return Ok((*page, false));
    }

    let new_page = alloc_clock_page()?;
    *page = new_page;
    Ok((new_page, true))
}

fn prepare_clock_pages_for_present_cpus() -> Result<Vec<ProcessorId>, SystemError> {
    ensure_kvm_clock_pages();
    let pages = kvm_clock_pages();
    let mut missing = Vec::new();
    for cpu_id in smp_cpu_manager().present_cpus().iter_cpu() {
        let slot = unsafe { pages.force_get_mut(cpu_id) };
        if !slot.is_valid() {
            missing.push(cpu_id);
        }
    }

    // Allocate into local staging first. No real per-CPU slot is changed until
    // every allocation and mapping has succeeded.
    let staged = allocate_missing_pages(&missing, &mut SystemClockPageAllocator)?;
    let mut committed = Vec::with_capacity(staged.len());
    for (cpu_id, page) in staged {
        let slot = unsafe { pages.force_get_mut(cpu_id) };
        debug_assert!(!slot.is_valid());
        *slot = page;
        committed.push(cpu_id);
    }
    Ok(committed)
}

fn rollback_clock_pages(cpu_ids: &[ProcessorId]) {
    let pages = kvm_clock_pages();
    let mut allocator = SystemClockPageAllocator;
    for &cpu_id in cpu_ids.iter().rev() {
        let slot = unsafe { pages.force_get_mut(cpu_id) };
        if slot.is_valid() {
            let page = *slot;
            *slot = KvmClockPage::empty();
            release_clock_page_with(&mut allocator, page);
        }
    }
}

fn this_cpu_pvti() -> Option<&'static PvclockVcpuTimeInfo> {
    let cpu_id = smp_get_processor_id();
    let page = unsafe { kvm_clock_pages().force_get(cpu_id) };
    if !page.is_valid() {
        return None;
    }

    let ptr = page.virt.data() as *const PvclockVsyscallTimeInfo;
    Some(unsafe { &(*ptr).pvti })
}

fn write_system_time_msr(phys: PhysAddr) {
    let msr = MSR_KVM_SYSTEM_TIME.load(Ordering::SeqCst);
    if msr == 0 {
        return;
    }

    let val = (phys.data() as u64) | 1u64;
    unsafe { wrmsr(msr, val) };
}

fn disable_system_time_msr() {
    let msr = MSR_KVM_SYSTEM_TIME.load(Ordering::SeqCst);
    if msr != 0 {
        unsafe { wrmsr(msr, 0) };
    }
}

fn kvm_clock_read() -> Option<u64> {
    let _guard = PreemptGuard::new();
    let pvti = this_cpu_pvti()?;
    Some(pvclock::pvclock_clocksource_read_nowd(pvti))
}

fn init_msrs() -> bool {
    let (system_time, wall_clock) = match kvm_para::kvm_clock_msrs() {
        Some(msrs) => msrs,
        None => return false,
    };

    MSR_KVM_SYSTEM_TIME.store(system_time, Ordering::SeqCst);
    let _ = wall_clock;
    true
}

fn update_tsc_khz_from_pvclock() {
    if let Some(pvti) = this_cpu_pvti() {
        let khz = pvclock::pvclock_tsc_khz(pvti);
        if khz != 0 {
            TSCManager::set_khz_from_kvm(khz);
        }
    }
}

pub fn kvmclock_init() -> bool {
    if KVM_CLOCK_READY.load(Ordering::SeqCst) {
        return true;
    }

    if !kvm_para::kvm_para_available() {
        return false;
    }

    if !init_msrs() {
        return false;
    }

    let mut committed_pages = match prepare_clock_pages_for_present_cpus() {
        Ok(committed) => committed,
        Err(e) => {
            warn!("kvm-clock: prealloc pvclock pages failed: {:?}", e);
            return false;
        }
    };
    let (page, newly_allocated) = match ensure_clock_page_for_cpu() {
        Ok(result) => result,
        Err(e) => {
            warn!("kvm-clock: alloc clock page failed: {:?}", e);
            rollback_clock_pages(&committed_pages);
            return false;
        }
    };
    if newly_allocated {
        committed_pages.push(smp_get_processor_id());
    }

    if kvm_para::kvm_para_has_feature(kvm_para::KVM_FEATURE_CLOCKSOURCE_STABLE_BIT) {
        pvclock::pvclock_set_flags(PVCLOCK_TSC_STABLE_BIT);
    }

    write_system_time_msr(page.phys);

    let kvm_clock = KvmClock::new();
    let clock = kvm_clock.clone() as Arc<dyn Clocksource>;
    if let Err(e) = clock.register(1, NSEC_PER_SEC) {
        warn!("kvm-clock: register failed: {:?}", e);
        disable_system_time_msr();
        rollback_clock_pages(&committed_pages);
        return false;
    }

    KVM_CLOCK_READY.store(true, Ordering::SeqCst);

    update_tsc_khz_from_pvclock();

    info!("kvm-clock: registered");
    true
}

pub fn kvmclock_init_secondary() -> Result<(), SystemError> {
    if !KVM_CLOCK_READY.load(Ordering::SeqCst) {
        return Ok(());
    }

    let cpu_id = smp_get_processor_id();
    let page = unsafe { *kvm_clock_pages().force_get(cpu_id) };
    if !page.is_valid() {
        return Err(SystemError::ENODEV);
    }

    write_system_time_msr(page.phys);
    Ok(())
}

#[path = "kvm_clock_selftest.rs"]
mod selftest;

pub(crate) use selftest::run_kvm_clock_allocator_selftests;
