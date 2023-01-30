#![allow(dead_code)]
use core::arch::asm;

#[inline(always)]
pub fn mfence() {
    unsafe {
        asm!("mfence");
    }
}

#[inline(always)]
pub fn lfence() {
    unsafe {
        asm!("lfence");
    }
}

#[inline(always)]
pub fn sfence() {
    unsafe {
        asm!("sfence");
    }
}
