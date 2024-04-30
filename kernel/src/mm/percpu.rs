use core::sync::atomic::AtomicU32;

use alloc::vec::Vec;

use crate::{
    libs::lazy_init::Lazy,
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, ProcessorId},
    },
};

/// 系统中的CPU数量
///
/// todo: 待smp模块重构后，从smp模块获取CPU数量。
/// 目前由于smp模块初始化时机较晚，导致大部分内核模块无法在早期初始化PerCpu变量。
static CPU_NUM_ATOMIC: AtomicU32 = AtomicU32::new(PerCpu::MAX_CPU_NUM);

#[derive(Debug)]
pub struct PerCpu;

impl PerCpu {
    #[cfg(target_arch = "x86_64")]
    pub const MAX_CPU_NUM: u32 = 128;
    #[cfg(target_arch = "riscv64")]
    pub const MAX_CPU_NUM: u32 = 64;

    /// # 初始化PerCpu
    ///
    /// 该函数应该在内核初始化时调用一次。
    ///
    /// 该函数会调用`smp_get_total_cpu()`获取CPU数量，然后将其存储在`CPU_NUM`中。
    #[allow(dead_code)]
    pub fn init() {
        let cpu_num: &AtomicU32 = &CPU_NUM_ATOMIC;
        if cpu_num.load(core::sync::atomic::Ordering::SeqCst) != 0 {
            panic!("PerCpu::init() called twice");
        }
        let cpus = smp_cpu_manager().present_cpus_count();
        assert!(cpus > 0, "PerCpu::init(): present_cpus_count() returned 0");

        CPU_NUM_ATOMIC.store(cpus, core::sync::atomic::Ordering::SeqCst);
    }
}

/// PerCpu变量
///
/// 该结构体的每个实例都是线程安全的，因为每个CPU都有自己的变量。
///
/// 一种简单的使用方法是：使用该结构体提供的`define_lazy`方法定义一个全局变量，
/// 然后在内核初始化时调用`init`、`new`方法去初始化它。
///
/// 当然，由于Lazy<T>有运行时开销，所以也可以直接全局声明一个Option，
/// 然后手动初始化然后赋值到Option中。（这样需要在初始化的时候，手动确保并发安全）
#[derive(Debug)]
#[allow(dead_code)]
pub struct PerCpuVar<T> {
    inner: Vec<T>,
}

#[allow(dead_code)]
impl<T> PerCpuVar<T> {
    /// # 初始化PerCpu变量
    ///
    /// ## 参数
    ///
    /// - `data` - 每个CPU的数据的初始值。 传入的Vec的长度必须等于CPU的数量，否则返回None。
    pub fn new(data: Vec<T>) -> Option<Self> {
        let cpu_num = CPU_NUM_ATOMIC.load(core::sync::atomic::Ordering::SeqCst);
        if cpu_num == 0 {
            panic!("PerCpu::init() not called");
        }

        if data.len() != cpu_num.try_into().unwrap() {
            return None;
        }

        return Some(Self { inner: data });
    }

    /// 定义一个Lazy的PerCpu变量，稍后再初始化
    pub const fn define_lazy() -> Lazy<Self> {
        Lazy::<Self>::new()
    }

    pub fn get(&self) -> &T {
        let cpu_id = smp_get_processor_id();
        &self.inner[cpu_id.data() as usize]
    }

    #[allow(clippy::mut_from_ref)]
    pub fn get_mut(&self) -> &mut T {
        let cpu_id = smp_get_processor_id();
        unsafe {
            &mut (self as *const Self as *mut Self).as_mut().unwrap().inner[cpu_id.data() as usize]
        }
    }

    pub unsafe fn force_get(&self, cpu_id: ProcessorId) -> &T {
        &self.inner[cpu_id.data() as usize]
    }

    #[allow(clippy::mut_from_ref)]
    pub unsafe fn force_get_mut(&self, cpu_id: ProcessorId) -> &mut T {
        &mut (self as *const Self as *mut Self).as_mut().unwrap().inner[cpu_id.data() as usize]
    }
}

/// PerCpu变量是线程安全的，因为每个CPU都有自己的变量。
unsafe impl<T> Sync for PerCpuVar<T> {}
unsafe impl<T> Send for PerCpuVar<T> {}
