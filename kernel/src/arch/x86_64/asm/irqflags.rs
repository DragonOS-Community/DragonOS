use core::arch::asm;

#[inline]
pub fn local_irq_save() -> usize {
    let x: usize;
    unsafe {
        asm!("pushfq ; pop {} ; cli", out(reg) x, options(nostack));
    }
    x
}

#[inline]
// 恢复先前保存的rflags的值x
pub fn local_irq_restore(x: usize) {
    unsafe {
        asm!("push {} ; popfq", in(reg) x, options(nostack));
    }
}
