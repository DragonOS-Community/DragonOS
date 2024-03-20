use core::sync::atomic::AtomicU32;

use crate::libs::cpumask::CpuMask;

mod c_adapter;

int_like!(ProcessorId, AtomicProcessorId, u32, AtomicU32);

impl ProcessorId {
    pub const INVALID: ProcessorId = ProcessorId::new(u32::MAX);
}

static mut SMP_CPU_MANAGER: Option<SmpCpuManager> = None;

#[inline]
pub fn smp_cpu_manager() -> &'static SmpCpuManager {
    unsafe { SMP_CPU_MANAGER.as_ref().unwrap() }
}

pub struct SmpCpuManager {
    possible_cpus: CpuMask,
}

impl SmpCpuManager {
    fn new() -> Self {
        let possible_cpus = CpuMask::new();
        Self { possible_cpus }
    }

    /// 设置可用的CPU
    ///
    /// # Safety
    ///
    /// - 该函数不会检查CPU的有效性，调用者需要保证CPU的有效性。
    /// - 由于possible_cpus是一个全局变量，且为了性能考虑，并不会加锁
    ///     访问，因此该函数只能在初始化阶段调用。
    pub unsafe fn set_possible_cpu(&self, cpu: ProcessorId, value: bool) {
        // 强制获取mut引用，因为该函数只能在初始化阶段调用
        let p = (self as *const Self as *mut Self).as_mut().unwrap();

        p.possible_cpus.set(cpu, value);
    }

    /// 获取可用的CPU
    #[allow(dead_code)]
    pub fn possible_cpus(&self) -> &CpuMask {
        &self.possible_cpus
    }
}

pub fn smp_cpu_manager_init(boot_cpu: ProcessorId) {
    unsafe {
        SMP_CPU_MANAGER = Some(SmpCpuManager::new());
    }

    unsafe { smp_cpu_manager().set_possible_cpu(boot_cpu, true) };

    SmpCpuManager::arch_init(boot_cpu);
}
