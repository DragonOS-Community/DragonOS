use crate::arch::CurrentIrqArch;

pub mod softirq;

/// @brief 中断相关的操作
pub trait InterruptArch: Send + Sync {
    /// @brief 使能中断
    unsafe fn interrupt_enable();
    /// @brief 禁止中断
    unsafe fn interrupt_disable();
    /// @brief 检查中断是否被禁止
    fn is_irq_enabled() -> bool;

    /// @brief 保存当前中断状态，并且禁止中断
    unsafe fn save_and_disable_irq() -> IrqFlagsGuard;
    unsafe fn restore_irq(flags: IrqFlags);
}

#[derive(Debug, Clone, Copy)]
pub struct IrqFlags {
    flags: u64,
}

impl IrqFlags {
    pub fn new(flags: u64) -> Self {
        IrqFlags { flags }
    }

    pub fn flags(&self) -> u64 {
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
