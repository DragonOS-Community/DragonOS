use loongArch64::register::{ecfg, eentry};

use crate::{init::init::start_kernel, mm::PhysAddr};

static mut BOOT_HARTID: u32 = 0;
static mut BOOT_FDT_PADDR: PhysAddr = PhysAddr::new(0);

#[no_mangle]
pub unsafe extern "C" fn kernel_main(hartid: usize, fdt_paddr: usize) -> ! {
    clear_bss();

    let fdt_paddr = PhysAddr::new(fdt_paddr);

    unsafe {
        BOOT_HARTID = hartid as u32;
        BOOT_FDT_PADDR = fdt_paddr;
    }
    setup_trap_vector();
    start_kernel();
}

/// 设置中断、异常处理函数
fn setup_trap_vector() {
    // todo!();
    // let ptr = handle_exception as *const () as usize;
    // ecfg::set_vs(0);
    // eentry::set_eentry(handle_exception as usize);
}

/// Clear the bss section
fn clear_bss() {
    extern "C" {
        fn _bss();
        fn _ebss();
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
