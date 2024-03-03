mod c_adapter;
pub(super) mod entry;
mod handle;
pub mod ipi;
pub mod msi;
pub mod trap;

use core::{
    arch::asm,
    sync::atomic::{compiler_fence, Ordering},
};

use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::{InterruptArch, IrqFlags, IrqFlagsGuard, IrqNumber},
    kerror,
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
        kerror!("Unexpected IRQ trap at vector {}", irq.data());
        CurrentApic.send_eoi();
    }

    fn arch_early_irq_init() -> Result<(), SystemError> {
        arch_early_irq_init()
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
    pub errcode: ::core::ffi::c_ulong,
    pub rip: ::core::ffi::c_ulong,
    pub cs: ::core::ffi::c_ulong,
    pub rflags: ::core::ffi::c_ulong,
    pub rsp: ::core::ffi::c_ulong,
    pub ss: ::core::ffi::c_ulong,
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
    pub fn from_user(&self) -> bool {
        if (self.cs & 0x3) != 0 {
            return true;
        } else {
            return false;
        }
    }
}
