use fdt::node::FdtNode;
use system_error::SystemError;

use crate::{
    arch::{driver::sbi::SbiDriver, mm::init::mm_early_init},
    driver::{firmware::efi::init::efi_init, open_firmware::fdt::open_firmware_fdt_driver},
    init::{boot_params, init::start_kernel},
    kdebug, kinfo,
    mm::{memblock::mem_block_manager, PhysAddr, VirtAddr},
    print, println,
};

use super::driver::sbi::console_putstr;

#[derive(Debug)]
pub struct ArchBootParams {
    /// 启动时的fdt物理地址
    pub fdt_paddr: PhysAddr,
    pub fdt_vaddr: Option<VirtAddr>,
}

impl ArchBootParams {
    pub const DEFAULT: Self = ArchBootParams {
        fdt_paddr: PhysAddr::new(0),
        fdt_vaddr: None,
    };

    pub fn arch_fdt(&self) -> VirtAddr {
        // 如果fdt_vaddr为None，则说明还没有进行内核虚拟地址空间的映射，此时返回物理地址
        if self.fdt_vaddr.is_none() {
            return VirtAddr::new(self.fdt_paddr.data());
        }
        self.fdt_vaddr.unwrap()
    }
}

static mut BOOT_HARTID: usize = 0;
static mut BOOT_FDT_PADDR: PhysAddr = PhysAddr::new(0);

#[no_mangle]
unsafe extern "C" fn kernel_main(hartid: usize, fdt_paddr: usize) -> ! {
    let fdt_paddr = PhysAddr::new(fdt_paddr);

    unsafe {
        BOOT_HARTID = hartid;
        BOOT_FDT_PADDR = fdt_paddr;
    }

    start_kernel();
}

#[inline(never)]
fn print_node(node: FdtNode<'_, '_>, n_spaces: usize) {
    (0..n_spaces).for_each(|_| print!(" "));
    println!("{}/", node.name);
    node.properties().for_each(|p| {
        (0..n_spaces + 4).for_each(|_| print!(" "));
        println!("{}: {:?}", p.name, p.value);
    });

    for child in node.children() {
        print_node(child, n_spaces + 4);
    }
}

/// 解析fdt，获取内核启动参数
#[inline(never)]
unsafe fn parse_dtb() {
    let fdt_paddr = boot_params().read().arch.fdt_paddr;
    if fdt_paddr.is_null() {
        panic!("Failed to get fdt address!");
    }

    open_firmware_fdt_driver()
        .early_scan_device_tree()
        .expect("Failed to scan device tree at boottime.");
}

#[inline(never)]
pub fn early_setup_arch() -> Result<(), SystemError> {
    SbiDriver::early_init();
    let hartid: usize = unsafe { BOOT_HARTID };
    let fdt_paddr = unsafe { BOOT_FDT_PADDR };
    boot_params().write().arch.fdt_paddr = fdt_paddr;

    kinfo!(
        "DragonOS kernel is running on hart {}, fdt address:{:?}",
        hartid,
        fdt_paddr
    );
    mm_early_init();

    let fdt =
        unsafe { fdt::Fdt::from_ptr(fdt_paddr.data() as *const u8).expect("Failed to parse fdt!") };
    print_node(fdt.find_node("/").unwrap(), 0);

    unsafe { parse_dtb() };

    for x in mem_block_manager().to_iter() {
        kdebug!("before efi: {x:?}");
    }

    efi_init();

    open_firmware_fdt_driver().early_init_fdt_scan_reserved_mem();

    return Ok(());
}

#[inline(never)]
pub fn setup_arch() -> Result<(), SystemError> {
    // todo
    return Ok(());
}

#[inline(never)]
pub fn setup_arch_post() -> Result<(), SystemError> {
    // todo
    return Ok(());
}
