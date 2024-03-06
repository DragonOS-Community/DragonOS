use core::{
    arch::asm,
    hint::spin_loop,
    sync::atomic::{compiler_fence, AtomicBool, Ordering},
};

use kdepends::memoffset::offset_of;
use system_error::SystemError;

use crate::{
    arch::process::table::TSSManager,
    exception::InterruptArch,
    include::bindings::bindings::{cpu_core_info, smp_init},
    kdebug,
    libs::rwlock::RwLock,
    mm::percpu::PerCpu,
    process::ProcessManager,
    smp::{core::smp_get_processor_id, cpu::ProcessorId, SMPArch},
};

use super::{acpi::early_acpi_boot_init, CurrentIrqArch};

extern "C" {
    fn smp_ap_start_stage2();
}

pub(super) static X86_64_SMP_MANAGER: X86_64SmpManager = X86_64SmpManager::new();

#[repr(C)]
struct ApStartStackInfo {
    vaddr: usize,
}

/// AP处理器启动时执行
#[no_mangle]
unsafe extern "C" fn smp_ap_start() -> ! {
    CurrentIrqArch::interrupt_disable();
    let vaddr = cpu_core_info[smp_get_processor_id().data() as usize].stack_start as usize;
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    let v = ApStartStackInfo { vaddr };
    smp_init_switch_stack(&v);
}

#[naked]
unsafe extern "sysv64" fn smp_init_switch_stack(st: &ApStartStackInfo) -> ! {
    asm!(concat!("
        mov rsp, [rdi + {off_rsp}]
        mov rbp, [rdi + {off_rsp}]
        jmp {stage1}
    "), 
        off_rsp = const(offset_of!(ApStartStackInfo, vaddr)),
        stage1 = sym smp_ap_start_stage1, 
    options(noreturn));
}

unsafe extern "C" fn smp_ap_start_stage1() -> ! {
    let id = smp_get_processor_id();
    kdebug!("smp_ap_start_stage1: id: {}\n", id.data());
    let current_idle = ProcessManager::idle_pcb()[smp_get_processor_id().data() as usize].clone();

    let tss = TSSManager::current_tss();

    tss.set_rsp(
        x86::Ring::Ring0,
        current_idle.kernel_stack().stack_max_address().data() as u64,
    );
    TSSManager::load_tr();

    smp_ap_start_stage2();
    loop {
        spin_loop();
    }
}

/// 多核的数据
#[derive(Debug)]
pub struct SmpBootData {
    initialized: AtomicBool,
    cpu_count: usize,
    /// CPU的物理ID（指的是Local APIC ID）
    ///
    /// 这里必须保证第0项的是bsp的物理ID
    phys_id: [usize; PerCpu::MAX_CPU_NUM as usize],
}

#[allow(dead_code)]
impl SmpBootData {
    pub fn cpu_count(&self) -> usize {
        self.cpu_count
    }

    /// 获取CPU的物理ID
    pub fn phys_id(&self, cpu_id: usize) -> usize {
        self.phys_id[cpu_id]
    }

    /// 获取BSP的物理ID
    pub fn bsp_phys_id(&self) -> usize {
        self.phys_id[0]
    }

    pub unsafe fn set_cpu_count(&self, cpu_count: u32) {
        if self.initialized.load(Ordering::SeqCst) == false {
            let p = self as *const SmpBootData as *mut SmpBootData;
            (*p).cpu_count = cpu_count.try_into().unwrap();
        }
    }

    pub unsafe fn set_phys_id(&self, cpu_id: ProcessorId, phys_id: usize) {
        if self.initialized.load(Ordering::SeqCst) == false {
            let p = self as *const SmpBootData as *mut SmpBootData;
            (*p).phys_id[cpu_id.data() as usize] = phys_id;
        }
    }

    /// 标记boot data结构体已经初始化完成
    pub fn mark_initialized(&self) {
        self.initialized.store(true, Ordering::SeqCst);
    }
}

pub(super) static SMP_BOOT_DATA: SmpBootData = SmpBootData {
    initialized: AtomicBool::new(false),
    cpu_count: 0,
    phys_id: [0; PerCpu::MAX_CPU_NUM as usize],
};

#[allow(dead_code)]
#[derive(Debug)]
pub struct X86_64SmpManager {
    ia64_cpu_to_sapicid: RwLock<[Option<usize>; PerCpu::MAX_CPU_NUM as usize]>,
}

impl X86_64SmpManager {
    pub const fn new() -> Self {
        return Self {
            ia64_cpu_to_sapicid: RwLock::new([None; PerCpu::MAX_CPU_NUM as usize]),
        };
    }
    /// initialize the logical cpu number to APIC ID mapping
    pub fn build_cpu_map(&self) -> Result<(), SystemError> {
        // 参考：https://code.dragonos.org.cn/xref/linux-6.1.9/arch/ia64/kernel/smpboot.c?fi=smp_build_cpu_map#496
        // todo!("build_cpu_map")
        return Ok(());
    }
}

pub struct X86_64SMPArch;

impl SMPArch for X86_64SMPArch {
    #[inline(never)]
    fn prepare_cpus() -> Result<(), SystemError> {
        early_acpi_boot_init()?;
        X86_64_SMP_MANAGER.build_cpu_map()?;
        return Ok(());
    }

    #[inline(never)]
    fn init() -> Result<(), SystemError> {
        x86::fence::mfence();
        unsafe { smp_init() };
        x86::fence::mfence();
        return Ok(());
    }
}
