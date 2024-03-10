use fdt::node::FdtNode;
use system_error::SystemError;

use crate::{
    arch::{driver::sbi::SbiDriver, mm::init::mm_early_init},
    driver::{firmware::efi::init::efi_init, open_firmware::fdt::open_firmware_fdt_driver},
    init::{boot_params, init::start_kernel},
    kdebug, kinfo,
    mm::{memblock::mem_block_manager, PhysAddr, VirtAddr},
    print, println,
    smp::cpu::ProcessorId,
};

use super::{cpu::init_local_context, interrupt::entry::handle_exception};

#[derive(Debug)]
pub struct ArchBootParams {
    /// 启动时的fdt物理地址
    pub fdt_paddr: PhysAddr,
    pub fdt_vaddr: Option<VirtAddr>,

    pub boot_hartid: ProcessorId,
}

impl ArchBootParams {
    pub const DEFAULT: Self = ArchBootParams {
        fdt_paddr: PhysAddr::new(0),
        fdt_vaddr: None,
        boot_hartid: ProcessorId::new(0),
    };

    pub fn arch_fdt(&self) -> VirtAddr {
        // 如果fdt_vaddr为None，则说明还没有进行内核虚拟地址空间的映射，此时返回物理地址
        if self.fdt_vaddr.is_none() {
            return VirtAddr::new(self.fdt_paddr.data());
        }
        self.fdt_vaddr.unwrap()
    }
}

static mut BOOT_HARTID: u32 = 0;
static mut BOOT_FDT_PADDR: PhysAddr = PhysAddr::new(0);

#[no_mangle]
unsafe extern "C" fn kernel_main(hartid: usize, fdt_paddr: usize) -> ! {
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
    let ptr = handle_exception as *const () as usize;

    unsafe {
        riscv::register::stvec::write(ptr, riscv::register::stvec::TrapMode::Direct);
        // Set sup0 scratch register to 0, indicating to exception vector that
        // we are presently executing in kernel.
        riscv::register::sscratch::write(0);
    }
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
    let hartid = unsafe { BOOT_HARTID };
    let fdt_paddr = unsafe { BOOT_FDT_PADDR };

    let mut arch_boot_params_guard = boot_params().write();
    arch_boot_params_guard.arch.fdt_paddr = fdt_paddr;
    arch_boot_params_guard.arch.boot_hartid = ProcessorId::new(hartid);

    drop(arch_boot_params_guard);

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
    init_local_context();
    return Ok(());
}

#[inline(never)]
pub fn setup_arch_post() -> Result<(), SystemError> {
    // todo
    return Ok(());
}
