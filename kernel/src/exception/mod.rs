use core::ops::Add;

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
pub mod manage;
pub mod msi;
mod resend;
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

impl IrqNumber {
    /// 如果一个(PCI)设备中断没有被连接，我们将设置irqnumber为IRQ_NOTCONNECTED。
    /// 这导致request_irq()失败，返回-ENOTCONN，这样我们就可以区分这种情况和其他错误返回。
    pub const IRQ_NOTCONNECTED: IrqNumber = IrqNumber::new(u32::MAX);
}

// 硬件中断号
// 用于表示在某个IrqDomain中的中断号
int_like!(HardwareIrqNumber, u32);

impl Add<u32> for HardwareIrqNumber {
    type Output = HardwareIrqNumber;

    fn add(self, rhs: u32) -> HardwareIrqNumber {
        HardwareIrqNumber::new(self.0 + rhs)
    }
}

impl Add<u32> for IrqNumber {
    type Output = IrqNumber;

    fn add(self, rhs: u32) -> IrqNumber {
        IrqNumber::new(self.0 + rhs)
    }
}
