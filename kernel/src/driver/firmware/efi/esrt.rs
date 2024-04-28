//! 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/firmware/efi/esrt.c#1

use super::efi_manager;

#[inline(never)]
pub(super) fn efi_esrt_init() {
    if !efi_manager().esrt_table_exists() {
        return;
    }

    // todo: 参考linux 的 `efi_esrt_init`来实现
    todo!("efi_esrt_init")
}
