#![allow(function_casts_as_integer)]

use crate::arch::MMArch;
use crate::init::boot_params;
use crate::init::kexec::Kimage;
use crate::libs::spinlock::SpinLock;
use crate::mm::ident_map::{ident_map_page, ident_map_pages, ident_pt_alloc};
use crate::mm::kernel_mapper::KernelMapper;
use crate::mm::MemoryManagementArch;
use crate::mm::{page::EntryFlags, PhysAddr};
use alloc::rc::Rc;
use core::mem::transmute;
use system_error::SystemError;

type RelocateKernelFn =
    unsafe extern "C" fn(indirection_page: usize, start_address: usize, stack_page_address: usize);

pub fn machine_kexec_prepare(kimage: Rc<SpinLock<Kimage>>) -> Result<(), SystemError> {
    unsafe {
        unsafe extern "C" {
            unsafe fn __relocate_kernel_start();
            unsafe fn __relocate_kernel_end();
        }
        let reloc_start = __relocate_kernel_start as usize;
        let reloc_end = __relocate_kernel_end as usize;

        if reloc_end - reloc_start > MMArch::PAGE_SIZE {
            panic!("Kexec: relocate_kernel func is bigger than PAGE_SIZE");
        }

        let control_page_phys = kimage
            .lock()
            .control_code_page
            .clone()
            .unwrap()
            .phys_address();
        let virt = MMArch::phys_2_virt(control_page_phys).unwrap().data();

        core::ptr::copy(
            reloc_start as *mut u8,
            virt as *mut u8,
            reloc_end - reloc_start,
        );

        // 搬运 kernel_cmdline
        // Linux 下 boot_params(zero page) 会被加载到 0x1000, 覆盖当前 x86 的 bootloader pvh 的参数范围(0x11a0)
        // 这里与 Linux 相同, 写死放到 0x20000
        let cmdline_ptr = boot_params().read().arch.hdr.cmd_line_ptr as usize;
        let phys = PhysAddr::new(cmdline_ptr);
        let virt = MMArch::phys_2_virt(phys).unwrap();
        let mut kernel_mapper = KernelMapper::lock();
        kernel_mapper
            .map_phys_with_size(
                virt,
                phys,
                MMArch::PAGE_SIZE,
                EntryFlags::from_data(
                    MMArch::ENTRY_FLAG_PRESENT
                        | MMArch::ENTRY_FLAG_READWRITE
                        | MMArch::ENTRY_FLAG_GLOBAL
                        | MMArch::ENTRY_FLAG_DIRTY
                        | MMArch::ENTRY_FLAG_ACCESSED,
                ),
                true,
            )
            .unwrap();
        let slice = core::slice::from_raw_parts_mut(virt.data() as *mut u8, 2048);
        slice.fill(0);
        // 这里先使用固定的写死的 cmdline, 后续等 DragonOS 的切换设置没问题了让 linux 能完成初始化的时候改成与 DragonOS 一样就行
        let mess = "console=ttyS0 earlyprintk=serial,ttyS0,115200";
        let mut mess_buf = mess.as_bytes().to_vec();
        mess_buf.resize(2048, 0);
        slice.copy_from_slice(&mess_buf);
    }
    Ok(())
}

pub fn init_pgtable(kimage: Rc<SpinLock<Kimage>>) -> Result<(), SystemError> {
    let pgd = ident_pt_alloc();
    kimage.lock().pgd = pgd;

    unsafe extern "C" {
        pub unsafe static mut kexec_pa_table_page: u64;
    }

    unsafe {
        kexec_pa_table_page = pgd as u64;
    }

    let nr_segments = kimage.lock().nr_segments;

    // mems
    for i in 0..nr_segments {
        let addr = kimage.lock().segment[i].mem;
        let size = kimage.lock().segment[i].memsz;
        // TODO:处理可能不是页面整数的情况, 但是目前, 传入的参数都是在用户层页面对其和取整了
        let pages_nums = size / MMArch::PAGE_SIZE;
        ident_map_pages(pgd, addr, addr, pages_nums)?;
    }

    // pages
    // 这里需要说明一下, linux 中的操作为使用 GFP_HIGHUSER 分配的页面, 位于高内存(如 4G 空间中的 2 - 4G 高地址空间)
    // 详细代码为https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/kexec_core.c#802
    // 随后 linux 把 pfn 映射了, 在https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/machine_kexec_64.c#219
    // 我个人打点日志输出 linux 映射的区域为 [0, 7ffdf000] 和 [100000000, 180000000], 分别为低 2G 和高 2G (qemu启动为4G)
    // 其中 [100000000, 180000000] 会影响程序 kexec 的运行(如果注释掉那么就不能切内核了)
    // 但是目前 DragonOS 没有这么细的管理, 甚至内存分配都不支持 flags, 所以先这么用着
    let len = kimage.lock().pages.len();
    for i in 0..len {
        let page = kimage.lock().pages[i].clone();
        let addr = page.phys_address().data();
        ident_map_page(pgd, addr, addr)?;
    }

    // efi
    // map_efi_systab()

    // ACPI
    // map_acpi_tables()

    // control_page
    let control_page_pa = kimage
        .lock()
        .control_code_page
        .clone()
        .unwrap()
        .phys_address();
    ident_map_page(
        pgd,
        unsafe { MMArch::phys_2_virt(control_page_pa).unwrap().data() },
        control_page_pa.data(),
    )?;

    // cmdline
    let cmdline_ptr = boot_params().read().arch.hdr.cmd_line_ptr as usize;
    ident_map_page(pgd, cmdline_ptr, cmdline_ptr)
}

pub fn machine_kexec(kimage: Rc<SpinLock<Kimage>>) {
    unsafe extern "C" {
        unsafe fn relocate_kernel();
        unsafe fn __relocate_kernel_start();
    }

    let control_page_virt = unsafe {
        MMArch::phys_2_virt(
            kimage
                .lock()
                .control_code_page
                .clone()
                .unwrap()
                .phys_address(),
        )
        .unwrap()
        .data()
    };
    let relocate_kernel_ptr: usize =
        control_page_virt + relocate_kernel as usize - __relocate_kernel_start as usize;

    let relocate_kernel_func: RelocateKernelFn = unsafe { transmute(relocate_kernel_ptr) };

    let arg1 = kimage.lock().head;
    let arg2 = kimage.lock().start;
    let arg3 = kimage
        .lock()
        .stack_page
        .clone()
        .unwrap()
        .phys_address()
        .data();

    unsafe { relocate_kernel_func(arg1, arg2, arg3) };

    panic!("Kexec should not run to here!");
}
