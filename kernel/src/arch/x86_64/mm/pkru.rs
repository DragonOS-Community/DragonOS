use alloc::sync::Arc;

use crate::mm::ucontext::LockedVMA;

const VM_PKEY_SHIFT: usize = 32;

/// X86_64架构的ProtectionKey使用32、33、34、35四个比特位
const PKEY_MASK: usize = 1 << 32 | 1 << 33 | 1 << 34 | 1 << 35;

/// 获取vma的protection_key
///
/// ## 参数
///
/// - `vma`: VMA
///
/// ## 返回值
/// - `u16`: vma的protection_key
pub fn vma_pkey(vma: Arc<LockedVMA>) -> u16 {
    let guard = vma.lock_irqsave();
    ((guard.vm_flags().bits() & PKEY_MASK) >> VM_PKEY_SHIFT) as u16
}

// TODO pkru实现参考：https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/include/asm/pkru.h

const PKRU_AD_BIT: u16 = 0x1;
const PKRU_WD_BIT: u16 = 0x2;
const PKRU_BITS_PER_PKEY: u32 = 2;

pub fn pkru_allows_pkey(pkey: u16, write: bool) -> bool {
    let pkru = read_pkru();

    if !pkru_allows_read(pkru, pkey) {
        return false;
    }
    if write & !pkru_allows_write(pkru, pkey) {
        return false;
    }

    true
}

pub fn pkru_allows_read(pkru: u32, pkey: u16) -> bool {
    let pkru_pkey_bits: u32 = pkey as u32 * PKRU_BITS_PER_PKEY;
    pkru & ((PKRU_AD_BIT as u32) << pkru_pkey_bits) > 0
}

pub fn pkru_allows_write(pkru: u32, pkey: u16) -> bool {
    let pkru_pkey_bits: u32 = pkey as u32 * PKRU_BITS_PER_PKEY;
    pkru & (((PKRU_AD_BIT | PKRU_WD_BIT) as u32) << pkru_pkey_bits) > 0
}

pub fn read_pkru() -> u32 {
    // TODO 实现读取pkru逻辑
    // https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/include/asm/pkru.h?fi=read_pkru#read_pkru
    0
}
