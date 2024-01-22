use core::intrinsics::unreachable;

use fdt::node::FdtNode;

use crate::{
    arch::{mm::init::mm_early_init, MMArch},
    driver::{
        firmware::efi::init::efi_init, open_firmware::fdt::open_firmware_fdt_driver,
        tty::serial::serial8250::send_to_default_serial8250_port,
    },
    init::{boot_params, init_before_mem_init},
    kdebug, kinfo,
    mm::{MemoryManagementArch, PhysAddr, VirtAddr},
    print, println,
};

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

#[no_mangle]
unsafe extern "C" fn kernel_main(hartid: usize, fdt_paddr: usize) -> ! {
    let fdt_paddr = PhysAddr::new(fdt_paddr);

    init_before_mem_init();
    extern "C" {
        fn BSP_IDLE_STACK_SPACE();
    }
    kdebug!("BSP_IDLE_STACK_SPACE={:#x}", BSP_IDLE_STACK_SPACE as u64);
    kdebug!("PAGE_ADDRESS_SIZE={}", MMArch::PAGE_ADDRESS_SIZE);
    kdebug!("PAGE_ADDRESS_SHIFT={}", MMArch::PAGE_ADDRESS_SHIFT);

    boot_params().write().arch.fdt_paddr = fdt_paddr;
    kinfo!(
        "DragonOS kernel is running on hart {}, fdt address:{:?}",
        hartid,
        fdt_paddr
    );

    mm_early_init();

    let fdt = fdt::Fdt::from_ptr(fdt_paddr.data() as *const u8).expect("Failed to parse fdt!");
    print_node(fdt.find_node("/").unwrap(), 0);

    parse_dtb();

    efi_init();

    loop {}
    unreachable()
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
