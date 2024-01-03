use core::intrinsics::unreachable;

use fdt::node::FdtNode;

use crate::{
    driver::open_firmware::fdt::open_firmware_fdt_driver,
    init::{boot_params, init_before_mem_init},
    kinfo,
    mm::{PhysAddr, VirtAddr},
    print, println,
};

#[derive(Debug)]
pub struct ArchBootParams {
    /// 启动时的fdt物理地址
    pub fdt_paddr: PhysAddr,
}

impl ArchBootParams {
    pub const DEFAULT: Self = ArchBootParams {
        fdt_paddr: PhysAddr::new(0),
    };
}

#[no_mangle]
unsafe extern "C" fn kernel_main(hartid: usize, fdt_paddr: usize) -> ! {
    let fdt_paddr = PhysAddr::new(fdt_paddr);
    init_before_mem_init();
    boot_params().write().arch.fdt_paddr = fdt_paddr;
    kinfo!(
        "DragonOS kernel is running on hart {}, fdt address:{:?}",
        hartid,
        fdt_paddr
    );

    let fdt = fdt::Fdt::from_ptr(fdt_paddr.data() as *const u8).expect("Failed to parse fdt!");
    print_node(fdt.find_node("/").unwrap(), 0);

    parse_dtb();

    loop {}
    unreachable()
}

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
unsafe fn parse_dtb() {
    let fdt_paddr = boot_params().read().arch.fdt_paddr;
    if fdt_paddr.is_null() {
        panic!("Failed to get fdt address!");
    }

    open_firmware_fdt_driver()
        .set_fdt_vaddr(VirtAddr::new(fdt_paddr.data()))
        .unwrap();
    open_firmware_fdt_driver()
        .early_scan_device_tree()
        .expect("Failed to scan device tree at boottime.");
}
