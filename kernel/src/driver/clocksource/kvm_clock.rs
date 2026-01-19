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
    time::{
        clocksource::{Clocksource, ClocksourceData, ClocksourceFlags, ClocksourceMask, CycleNum},
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
static mut CLOCKSOURCE_KVM: Option<Arc<KvmClock>> = None;

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
            max_idle_ns: Default::default(),
            flags: ClocksourceFlags::CLOCK_SOURCE_IS_CONTINUOUS,
            watchdog_last: CycleNum::new(0),
            cs_last: CycleNum::new(0),
            uncertainty_margin: 0,
            maxadj: 0,
            cycle_last: CycleNum::new(0),
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
        let now = kvm_clock_read().unwrap_or(0);
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

    fn update_clocksource_data(&self, data: ClocksourceData) -> Result<(), SystemError> {
        let d = &mut self.0.lock_irqsave().data;
        d.set_name(data.name);
        d.set_rating(data.rating);
        d.set_mask(data.mask);
        d.set_mult(data.mult);
        d.set_shift(data.shift);
        d.set_max_idle_ns(data.max_idle_ns);
        d.set_flags(data.flags);
        d.watchdog_last = data.watchdog_last;
        d.cs_last = data.cs_last;
        d.set_uncertainty_margin(data.uncertainty_margin);
        d.set_maxadj(data.maxadj);
        d.cycle_last = data.cycle_last;
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

fn alloc_clock_page() -> Result<KvmClockPage, SystemError> {
    let phys = unsafe { crate::arch::mm::LockedFrameAllocator.allocate_one() }
        .ok_or(SystemError::ENOMEM)?;
    let virt = unsafe { MMArch::phys_2_virt(phys) }.ok_or(SystemError::EINVAL)?;

    unsafe {
        MMArch::write_bytes(virt, 0, MMArch::PAGE_SIZE);
    }

    Ok(KvmClockPage { phys, virt })
}

fn ensure_clock_page_for_cpu() -> Result<KvmClockPage, SystemError> {
    ensure_kvm_clock_pages();
    let cpu_id = smp_get_processor_id();
    let pages = kvm_clock_pages();

    let page = unsafe { pages.force_get_mut(cpu_id) };
    if page.is_valid() {
        return Ok(*page);
    }

    let new_page = alloc_clock_page()?;
    *page = new_page;
    Ok(new_page)
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

    ensure_kvm_clock_pages();
    let page = match ensure_clock_page_for_cpu() {
        Ok(page) => page,
        Err(e) => {
            warn!("kvm-clock: alloc clock page failed: {:?}", e);
            return false;
        }
    };

    if kvm_para::kvm_para_has_feature(kvm_para::KVM_FEATURE_CLOCKSOURCE_STABLE_BIT) {
        pvclock::pvclock_set_flags(PVCLOCK_TSC_STABLE_BIT);
    }

    write_system_time_msr(page.phys);

    let kvm_clock = KvmClock::new();
    let clock = kvm_clock.clone() as Arc<dyn Clocksource>;
    if let Err(e) = clock.register(1, NSEC_PER_SEC) {
        warn!("kvm-clock: register failed: {:?}", e);
        return false;
    }

    unsafe {
        CLOCKSOURCE_KVM = Some(kvm_clock);
    }
    KVM_CLOCK_READY.store(true, Ordering::SeqCst);

    update_tsc_khz_from_pvclock();

    info!("kvm-clock: registered");
    true
}

pub fn kvmclock_init_secondary() {
    if !KVM_CLOCK_READY.load(Ordering::SeqCst) {
        return;
    }

    let page = match ensure_clock_page_for_cpu() {
        Ok(page) => page,
        Err(_) => return,
    };

    write_system_time_msr(page.phys);
}
