#![allow(dead_code)]
use core::arch::asm;

/// @brief 关闭中断
#[inline]
pub fn cli(){
    unsafe{
        asm!("cli");
    }
}

/// @brief 开启中断
#[inline]
pub fn sti(){
    unsafe{
        asm!("sti");
    }
}