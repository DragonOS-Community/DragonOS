#![allow(dead_code)]

pub mod ipi;

use core::{
    arch::asm,
    sync::atomic::{compiler_fence, Ordering},
};

use crate::exception::{InterruptArch, IrqFlags, IrqFlagsGuard};

use super::asm::irqflags::{local_irq_restore, local_irq_save};

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
        return rflags & (1 << 9) != 0;
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
}
