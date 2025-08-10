pub(super) mod entry;
mod handle;
pub mod ipi;
pub mod msi;
pub mod trap;

use core::any::Any;
use core::{
    arch::asm,
    sync::atomic::{compiler_fence, Ordering},
};
use kprobe::ProbeArgs;
use log::error;
use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::{InterruptArch, IrqFlags, IrqFlagsGuard, IrqNumber},
};

use super::{
    asm::irqflags::{local_irq_restore, local_irq_save},
    driver::apic::{lapic_vector::arch_early_irq_init, CurrentApic, LocalAPIC},
};

/// @brief 关闭中断
#[inline]
pub fn cli() {
    unsafe {
        asm!("cli");
    }
}

/// @brief 开启中断
#[inline]
pub fn sti() {
    unsafe {
        asm!("sti");
    }
}

pub struct X86_64InterruptArch;

impl InterruptArch for X86_64InterruptArch {
    #[inline(never)]
    unsafe fn arch_irq_init() -> Result<(), SystemError> {
        CurrentIrqArch::interrupt_disable();

        return Ok(());
    }
    unsafe fn interrupt_enable() {
        sti();
    }

    unsafe fn interrupt_disable() {
        cli();
    }

    fn is_irq_enabled() -> bool {
        let rflags: u64;
        unsafe {
            asm!("pushfq; pop {}", out(reg) rflags, options(nomem, preserves_flags));
        }
        return (rflags & (1 << 9)) != 0;
    }

    unsafe fn save_and_disable_irq() -> IrqFlagsGuard {
        compiler_fence(Ordering::SeqCst);
        let rflags = local_irq_save();
        let flags = IrqFlags::new(rflags);
        let guard = IrqFlagsGuard::new(flags);
        compiler_fence(Ordering::SeqCst);
        return guard;
    }

    unsafe fn restore_irq(flags: IrqFlags) {
        compiler_fence(Ordering::SeqCst);
        local_irq_restore(flags.flags());
        compiler_fence(Ordering::SeqCst);
    }

    fn probe_total_irq_num() -> u32 {
        // todo: 从APIC获取
        // 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/apic/vector.c?r=&mo=19514&fi=704#704
        256
    }

    fn ack_bad_irq(irq: IrqNumber) {
        error!("Unexpected IRQ trap at vector {}", irq.data());
        CurrentApic.send_eoi();
    }

    fn arch_early_irq_init() -> Result<(), SystemError> {
        arch_early_irq_init()
    }

    fn arch_ap_early_irq_init() -> Result<(), SystemError> {
        if !CurrentApic.init_current_cpu() {
            return Err(SystemError::ENODEV);
        }

        Ok(())
    }
}

/// 中断栈帧结构体
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct TrapFrame {
    pub r15: ::core::ffi::c_ulong,
    pub r14: ::core::ffi::c_ulong,
    pub r13: ::core::ffi::c_ulong,
    pub r12: ::core::ffi::c_ulong,
    pub r11: ::core::ffi::c_ulong,
    pub r10: ::core::ffi::c_ulong,
    pub r9: ::core::ffi::c_ulong,
    pub r8: ::core::ffi::c_ulong,
    pub rbx: ::core::ffi::c_ulong,
    pub rcx: ::core::ffi::c_ulong,
    pub rdx: ::core::ffi::c_ulong,
    pub rsi: ::core::ffi::c_ulong,
    pub rdi: ::core::ffi::c_ulong,
    pub rbp: ::core::ffi::c_ulong,
    pub ds: ::core::ffi::c_ulong,
    pub es: ::core::ffi::c_ulong,
    pub rax: ::core::ffi::c_ulong,
    pub func: ::core::ffi::c_ulong,
    /// - 该字段在异常发生时，保存的是错误码
    /// - 在系统调用时，由系统调用入口函数将其设置为系统调用号
    pub errcode: ::core::ffi::c_ulong,
    pub rip: ::core::ffi::c_ulong,
    pub cs: ::core::ffi::c_ulong,
    pub rflags: ::core::ffi::c_ulong,
    pub rsp: ::core::ffi::c_ulong,
    pub ss: ::core::ffi::c_ulong,
}

impl Default for TrapFrame {
    fn default() -> Self {
        Self::new()
    }
}

impl TrapFrame {
    pub fn new() -> Self {
        Self {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            ds: 0,
            es: 0,
            rax: 0,
            func: 0,
            errcode: 0,
            rip: 0,
            cs: 0,
            rflags: 0,
            rsp: 0,
            ss: 0,
        }
    }

    /// 设置中断栈帧返回值
    pub fn set_return_value(&mut self, value: usize) {
        self.rax = value as u64;
    }

    /// 判断当前中断是否来自用户模式
    pub fn is_from_user(&self) -> bool {
        return (self.cs & 0x3) != 0;
    }
    /// 设置当前的程序计数器
    pub fn set_pc(&mut self, pc: usize) {
        self.rip = pc as u64;
    }

    /// 获取系统调用号
    ///
    /// # Safety
    /// 该函数只能在系统调用上下文中调用，
    /// 在其他上下文中，该函数返回值未定义
    pub unsafe fn syscall_nr(&self) -> Option<usize> {
        if self.errcode == u64::MAX {
            return None;
        }
        Some(self.errcode as usize)
    }

    /// 获取系统调用错误码
    ///
    /// # Safety
    /// 该函数只能在系统调用上下文中调用，
    /// 在其他上下文中，该函数返回值未定义
    ///
    /// # Returns
    /// 返回一个 `Option<SystemError>`，表示系统调用的错误码。
    pub unsafe fn syscall_error(&self) -> Option<SystemError> {
        let val = self.rax as i32;
        SystemError::from_posix_errno(val)
    }
}

impl ProbeArgs for TrapFrame {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn break_address(&self) -> usize {
        (self.rip - 1) as usize
    }

    fn debug_address(&self) -> usize {
        self.rip as usize
    }
}
