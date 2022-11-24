use core::arch::asm;

#[inline]
pub fn local_irq_save(flags: &mut u64) {
    unsafe {
        asm!(
            "pushfq",
            "pop rax",
            "mov rax, {0}",
            "cli",
            out(reg)(*flags),
        );
    }
}

#[inline]
pub fn local_irq_restore(flags: &u64) {
    let x = *flags;

    unsafe {
        asm!("push r15",
            "popfq", in("r15")(x));
    }
}
