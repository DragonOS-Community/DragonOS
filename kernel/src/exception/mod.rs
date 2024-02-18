use system_error::SystemError;

use crate::arch::CurrentIrqArch;

pub mod dummychip;
pub mod handle;
pub mod init;
pub mod ipi;
pub mod irqchip;
pub mod irqdata;
pub mod irqdesc;
pub mod irqdomain;
pub mod msi;
pub mod softirq;
pub mod sysfs;

/// 中断的架构相关的trait
pub trait InterruptArch: Send + Sync {
    /// 架构相关的中断初始化
    unsafe fn arch_irq_init() -> Result<(), SystemError>;
    /// 使能中断
    unsafe fn interrupt_enable();
    /// 禁止中断
    unsafe fn interrupt_disable();
    /// 检查中断是否被禁止
    fn is_irq_enabled() -> bool;

    /// 保存当前中断状态，并且禁止中断
    unsafe fn save_and_disable_irq() -> IrqFlagsGuard;
    unsafe fn restore_irq(flags: IrqFlags);

    /// 检测系统支持的中断总数
    fn probe_total_irq_num() -> u32;

    fn arch_early_irq_init() -> Result<(), SystemError> {
        Ok(())
    }

    /// 响应未注册的中断
    fn ack_bad_irq(irq: IrqNumber);
}

#[derive(Debug, Clone, Copy)]
pub struct IrqFlags {
    flags: usize,
}

impl IrqFlags {
    pub fn new(flags: usize) -> Self {
        IrqFlags { flags }
    }

    pub fn flags(&self) -> usize {
        self.flags
    }
}

/// @brief 当前中断状态的保护器，当该对象被drop时，会恢复之前的中断状态
///
/// # Example
///
/// ```
/// use crate::arch::CurrentIrqArch;
///
/// // disable irq and save irq state （这是唯一的获取IrqFlagsGuard的方法）
/// let guard = unsafe{CurrentIrqArch::save_and_disable_irq()};
///
/// // do something
///
/// // 销毁guard时，会恢复之前的中断状态
/// drop(guard);
///
/// ```
#[derive(Debug)]
pub struct IrqFlagsGuard {
    flags: IrqFlags,
}

impl IrqFlagsGuard {
    /// @brief 创建IrqFlagsGuard对象
    ///
    /// # Safety
    ///
    /// 该函数不安全，因为它不会检查flags是否是一个有效的IrqFlags对象, 而当它被drop时，会恢复flags中的中断状态
    ///
    /// 该函数只应被`CurrentIrqArch::save_and_disable_irq`调用
    pub unsafe fn new(flags: IrqFlags) -> Self {
        IrqFlagsGuard { flags }
    }
}
impl Drop for IrqFlagsGuard {
    fn drop(&mut self) {
        unsafe {
            CurrentIrqArch::restore_irq(self.flags);
        }
    }
}

// 定义中断号结构体
// 用于表示软件逻辑视角的中断号，全局唯一
int_like!(IrqNumber, u32);

// 硬件中断号
// 用于表示在某个IrqDomain中的中断号
int_like!(HardwareIrqNumber, u32);
