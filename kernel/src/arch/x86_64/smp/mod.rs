use core::{
    hint::spin_loop,
    sync::atomic::{compiler_fence, fence, AtomicBool, Ordering},
};

use kdepends::memoffset::offset_of;
use log::debug;
use system_error::SystemError;

use crate::{
    arch::{mm::LowAddressRemapping, process::table::TSSManager, MMArch},
    exception::InterruptArch,
    libs::{cpumask::CpuMask, rwlock::RwLock},
    mm::{percpu::PerCpu, MemoryManagementArch, PhysAddr, VirtAddr, IDLE_PROCESS_ADDRESS_SPACE},
    process::ProcessManager,
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, CpuHpCpuState, ProcessorId, SmpCpuManager},
        init::smp_ap_start_stage2,
        SMPArch,
    },
};

use super::{
    acpi::early_acpi_boot_init,
    interrupt::ipi::{ipi_send_smp_init, ipi_send_smp_startup},
    CurrentIrqArch,
};

extern "C" {
    /// AP处理器启动时，会将CR3设置为这个值
    pub static mut __APU_START_CR3: u64;
    fn _apu_boot_start();
    fn _apu_boot_end();
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

    let vaddr = if let Some(t) = smp_cpu_manager()
        .cpuhp_state(smp_get_processor_id())
        .thread()
    {
        t.kernel_stack_force_ref().stack_max_address().data() - 16
    } else {
        // 没有设置ap核心的栈，那么就进入死循环。
        loop {
            spin_loop();
        }
    };
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    let v = ApStartStackInfo { vaddr };
    smp_init_switch_stack(&v);
}

#[naked]
unsafe extern "sysv64" fn smp_init_switch_stack(st: &ApStartStackInfo) -> ! {
    core::arch::naked_asm!(concat!("
        mov rsp, [rdi + {off_rsp}]
        mov rbp, [rdi + {off_rsp}]
        jmp {stage1}
    "), 
        off_rsp = const(offset_of!(ApStartStackInfo, vaddr)),
        stage1 = sym smp_ap_start_stage1);
}

unsafe extern "C" fn smp_ap_start_stage1() -> ! {
    let id = smp_get_processor_id();
    debug!("smp_ap_start_stage1: id: {}\n", id.data());
    let current_idle = ProcessManager::idle_pcb()[smp_get_processor_id().data() as usize].clone();

    let tss = TSSManager::current_tss();

    tss.set_rsp(
        x86::Ring::Ring0,
        current_idle.kernel_stack().stack_max_address().data() as u64,
    );
    TSSManager::load_tr();

    CurrentIrqArch::arch_ap_early_irq_init().expect("arch_ap_early_irq_init failed");

    smp_ap_start_stage2();
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
        if !self.initialized.load(Ordering::SeqCst) {
            let p = self as *const SmpBootData as *mut SmpBootData;
            (*p).cpu_count = cpu_count.try_into().unwrap();
        }
    }

    pub unsafe fn set_phys_id(&self, cpu_id: ProcessorId, phys_id: usize) {
        if !self.initialized.load(Ordering::SeqCst) {
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
        unsafe {
            smp_cpu_manager().set_possible_cpu(ProcessorId::new(0), true);
            smp_cpu_manager().set_present_cpu(ProcessorId::new(0), true);
            smp_cpu_manager().set_online_cpu(ProcessorId::new(0));
        }

        for cpu in 1..SMP_BOOT_DATA.cpu_count() {
            unsafe {
                smp_cpu_manager().set_possible_cpu(ProcessorId::new(cpu as u32), true);
                smp_cpu_manager().set_present_cpu(ProcessorId::new(cpu as u32), true);
            }
        }

        print_cpus("possible", smp_cpu_manager().possible_cpus());
        print_cpus("present", smp_cpu_manager().present_cpus());
        return Ok(());
    }
}

fn print_cpus(s: &str, mask: &CpuMask) {
    let mut v = vec![];
    for cpu in mask.iter_cpu() {
        v.push(cpu.data());
    }

    debug!("{s}: cpus: {v:?}\n");
}

pub struct X86_64SMPArch;

impl SMPArch for X86_64SMPArch {
    #[inline(never)]
    fn prepare_cpus() -> Result<(), SystemError> {
        early_acpi_boot_init()?;
        X86_64_SMP_MANAGER.build_cpu_map()?;
        return Ok(());
    }

    fn post_init() -> Result<(), SystemError> {
        // AP核心启动完毕，取消低地址映射
        unsafe {
            LowAddressRemapping::unmap_at_low_address(
                &mut IDLE_PROCESS_ADDRESS_SPACE()
                    .write_irqsave()
                    .user_mapper
                    .utable,
                true,
            )
        }
        return Ok(());
    }

    fn start_cpu(cpu_id: ProcessorId, _cpu_hpstate: &CpuHpCpuState) -> Result<(), SystemError> {
        Self::copy_smp_start_code();

        fence(Ordering::SeqCst);
        ipi_send_smp_init();
        fence(Ordering::SeqCst);
        ipi_send_smp_startup(cpu_id)?;

        fence(Ordering::SeqCst);
        ipi_send_smp_startup(cpu_id)?;

        fence(Ordering::SeqCst);

        return Ok(());
    }
}

impl X86_64SMPArch {
    const SMP_CODE_START: usize = 0x20000;
    /// 复制SMP启动代码到0x20000处
    fn copy_smp_start_code() -> (VirtAddr, usize) {
        let apu_boot_size = Self::start_code_size();

        fence(Ordering::SeqCst);
        unsafe {
            core::ptr::copy(
                _apu_boot_start as *const u8,
                Self::SMP_CODE_START as *mut u8,
                apu_boot_size,
            )
        };
        fence(Ordering::SeqCst);

        return (VirtAddr::new(Self::SMP_CODE_START), apu_boot_size);
    }

    fn start_code_size() -> usize {
        let apu_boot_start = _apu_boot_start as usize;
        let apu_boot_end = _apu_boot_end as usize;
        let apu_boot_size = apu_boot_end - apu_boot_start;
        return apu_boot_size;
    }
}

impl SmpCpuManager {
    #[allow(static_mut_refs)]
    pub fn arch_init(_boot_cpu: ProcessorId) {
        assert!(smp_get_processor_id().data() == 0);
        // 写入APU_START_CR3，这个值会在AP处理器启动时设置到CR3寄存器
        let addr = IDLE_PROCESS_ADDRESS_SPACE()
            .read_irqsave()
            .user_mapper
            .utable
            .table()
            .phys();
        let vaddr = unsafe {
            MMArch::phys_2_virt(PhysAddr::new(&mut __APU_START_CR3 as *mut u64 as usize)).unwrap()
        };
        let ptr = vaddr.data() as *mut u64;
        unsafe { *ptr = addr.data() as u64 };

        // 添加低地址映射
        unsafe {
            LowAddressRemapping::remap_at_low_address(
                &mut IDLE_PROCESS_ADDRESS_SPACE()
                    .write_irqsave()
                    .user_mapper
                    .utable,
            )
        };
    }
}
