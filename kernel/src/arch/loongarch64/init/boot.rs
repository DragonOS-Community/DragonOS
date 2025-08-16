use core::sync::atomic::compiler_fence;

use loongArch64::register::{ecfg, eentry};

use crate::{arch::interrupt::entry::handle_reserved_, init::init::start_kernel, mm::PhysAddr};

static mut BOOT_HARTID: u32 = 0;
static mut BOOT_FDT_PADDR: PhysAddr = PhysAddr::new(0);

#[no_mangle]
#[inline(never)]
pub unsafe extern "C" fn kernel_main(hartid: usize, fdt_paddr: usize) -> ! {
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    clear_bss();
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    let fdt_paddr = PhysAddr::new(fdt_paddr);

    unsafe {
        BOOT_HARTID = hartid as u32;
        BOOT_FDT_PADDR = fdt_paddr;
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    boot_tmp_setup_trap_vector();
    start_kernel();
}

/// 临时设置中断、异常处理函数
///
/// 后续需要通过 https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#1085
/// 这里的这个函数来重新设置中断、异常处理函数
fn boot_tmp_setup_trap_vector() {
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    let ptr = handle_reserved_ as *const () as usize;

    ecfg::set_vs(0);
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    eentry::set_eentry(ptr);
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

/// Clear the bss section
fn clear_bss() {
    extern "C" {
        fn _bss();
        fn _ebss();
    }
    if _bss as usize == 0
        || _ebss as usize == 0
        || _bss as usize >= _ebss as usize
        || _ebss as usize - _bss as usize == 0
    {
        return; // BSS section is empty, nothing to clear
    }

    unsafe {
        let bss_start = _bss as *mut u8;
        let bss_end = _ebss as *mut u8;
        let bss_size = bss_end as usize - bss_start as usize;

        // Clear in chunks of u128 for efficiency
        let u128_count = bss_size / core::mem::size_of::<u128>();
        let u128_slice = core::slice::from_raw_parts_mut(bss_start as *mut u128, u128_count);
        u128_slice.fill(0);

        // Clear any remaining bytes
        let remaining_bytes = bss_size % core::mem::size_of::<u128>();

        if remaining_bytes > 0 {
            let remaining_start = bss_start.add(u128_count * core::mem::size_of::<u128>());
            let remaining_slice = core::slice::from_raw_parts_mut(remaining_start, remaining_bytes);
            remaining_slice.fill(0);
        }
    }
}
