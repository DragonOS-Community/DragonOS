use core::cmp;

use x86::{current::task::TaskStateSegment, segmentation::SegmentSelector, Ring};

use crate::{
    arch::{
        process::io_bitmap::{TaskIoBitmap, IO_BITMAP_BYTES, IO_BITMAP_TERMINATOR_BYTES},
        CurrentIrqArch,
    },
    exception::InterruptArch,
    mm::{percpu::PerCpu, VirtAddr},
    process::ProcessManager,
    smp::core::smp_get_processor_id,
};

// === 段选择子在GDT中的索引 ===
/// kernel code segment selector
pub const KERNEL_CS: SegmentSelector = SegmentSelector::new(1, Ring::Ring0);
/// kernel data segment selector
pub const KERNEL_DS: SegmentSelector = SegmentSelector::new(2, Ring::Ring0);
/// user data segment selector
pub const USER_DS: SegmentSelector = SegmentSelector::new(5, Ring::Ring3);
/// user code segment selector
/// 如果改这里，记得改syscall_64里面写死的常量
pub const USER_CS: SegmentSelector = SegmentSelector::new(6, Ring::Ring3);

const HARDWARE_TSS_SIZE: usize = core::mem::size_of::<TaskStateSegment>();
const IO_BITMAP_OFFSET_VALID: u16 = HARDWARE_TSS_SIZE as u16;
const IO_BITMAP_TOTAL_BYTES: usize = IO_BITMAP_BYTES + IO_BITMAP_TERMINATOR_BYTES;
const TSS_LIMIT: u16 = (HARDWARE_TSS_SIZE + IO_BITMAP_TOTAL_BYTES - 1) as u16;
const IO_BITMAP_OFFSET_INVALID: u16 = TSS_LIMIT + 1;

const _: () = assert!(HARDWARE_TSS_SIZE <= u16::MAX as usize);
const _: () = assert!(HARDWARE_TSS_SIZE + IO_BITMAP_TOTAL_BYTES <= u16::MAX as usize);
const _: () = assert!(TSS_LIMIT as usize <= 0x000f_ffff);

static mut TSS_MANAGER: TSSManager = TSSManager {
    tss: [DragonOsTss::new(); PerCpu::MAX_CPU_NUM as usize],
};

extern "C" {
    static mut GDT_Table: [u64; 512];
}

/// 切换fs和gs段寄存器
///
/// 由于需要return使得它生效，所以不能inline
#[inline(never)]
pub unsafe fn switch_fs_and_gs(fs: SegmentSelector, gs: SegmentSelector) {
    x86::segmentation::load_fs(fs);
    x86::segmentation::load_gs(gs);
}

#[derive(Clone, Copy, Debug)]
#[repr(C, align(4096))]
pub struct DragonOsTss {
    hw_tss: TaskStateSegment,
    io_bitmap: [u8; IO_BITMAP_TOTAL_BYTES],
    prev_sequence: u64,
    prev_max_bytes: usize,
}

impl DragonOsTss {
    pub const fn new() -> Self {
        let mut hw_tss = TaskStateSegment::new();
        hw_tss.iomap_base = IO_BITMAP_OFFSET_INVALID;

        Self {
            hw_tss,
            io_bitmap: [0xff; IO_BITMAP_TOTAL_BYTES],
            prev_sequence: 0,
            prev_max_bytes: 0,
        }
    }

    pub fn set_rsp(&mut self, pl: Ring, stack_ptr: u64) {
        self.hw_tss.set_rsp(pl, stack_ptr);
    }

    pub fn set_ist(&mut self, index: usize, stack_ptr: u64) {
        self.hw_tss.set_ist(index, stack_ptr);
    }

    fn invalidate_io_bitmap(&mut self) {
        self.hw_tss.iomap_base = IO_BITMAP_OFFSET_INVALID;
    }

    fn load_io_bitmap(&mut self, bitmap: &TaskIoBitmap) {
        if self.prev_sequence != bitmap.sequence() {
            let copy_len = cmp::max(self.prev_max_bytes, bitmap.max_bytes()).min(IO_BITMAP_BYTES);
            if copy_len > 0 {
                self.io_bitmap[..copy_len].copy_from_slice(&bitmap.bitmap()[..copy_len]);
            }
            self.io_bitmap[IO_BITMAP_BYTES] = 0xff;
            self.prev_sequence = bitmap.sequence();
            self.prev_max_bytes = bitmap.max_bytes();
        }

        self.hw_tss.iomap_base = IO_BITMAP_OFFSET_VALID;
    }
}

#[derive(Debug)]
pub struct TSSManager {
    tss: [DragonOsTss; PerCpu::MAX_CPU_NUM as usize],
}

impl TSSManager {
    /// 获取当前CPU的TSS
    pub unsafe fn current_tss() -> &'static mut DragonOsTss {
        &mut TSS_MANAGER.tss[smp_get_processor_id().data() as usize]
    }

    /// 加载当前CPU的TSS
    pub unsafe fn load_tr() {
        let index = (10 + smp_get_processor_id().data() * 2) as u16;
        let selector = SegmentSelector::new(index, Ring::Ring0);

        Self::set_tss_descriptor(index, VirtAddr::new(Self::current_tss() as *mut _ as usize));
        x86::task::load_tr(selector);
    }

    pub unsafe fn invalidate_io_bitmap() {
        Self::current_tss().invalidate_io_bitmap();
    }

    pub fn update_io_bitmap_from_current() {
        let bitmap = {
            let current = ProcessManager::current_pcb();
            let arch = current.arch_info_irqsave();
            arch.io_bitmap()
        };

        let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let tss = unsafe { Self::current_tss() };
        if let Some(bitmap) = bitmap {
            let guard = bitmap.lock_irqsave();
            tss.load_io_bitmap(&guard);
        } else {
            tss.invalidate_io_bitmap();
        }
    }

    #[allow(static_mut_refs)]
    unsafe fn set_tss_descriptor(index: u16, vaddr: VirtAddr) {
        let limit = TSS_LIMIT as u64;
        let gdt_vaddr = VirtAddr::new(&GDT_Table as *const _ as usize);

        let gdt: &mut [u64] = core::slice::from_raw_parts_mut(gdt_vaddr.data() as *mut u64, 512);

        let vaddr = vaddr.data() as u64;
        gdt[index as usize] = (limit & 0xffff)
            | ((vaddr & 0xffff) << 16)
            | (((vaddr >> 16) & 0xff) << 32)
            | (0x89 << 40)
            | (((limit >> 16) & 0xf) << 48)
            | (((vaddr >> 24) & 0xff) << 56);
        gdt[index as usize + 1] = (vaddr >> 32) & 0xffffffff;
    }
}
