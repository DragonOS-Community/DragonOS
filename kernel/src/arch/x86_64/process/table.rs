use x86::{current::task::TaskStateSegment, segmentation::SegmentSelector, Ring};

use crate::{
    mm::{percpu::PerCpu, VirtAddr},
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

static mut TSS_MANAGER: TSSManager = TSSManager::new();

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

#[derive(Debug)]
pub struct TSSManager {
    tss: [TaskStateSegment; PerCpu::MAX_CPU_NUM as usize],
}

impl TSSManager {
    const fn new() -> Self {
        return Self {
            tss: [TaskStateSegment::new(); PerCpu::MAX_CPU_NUM as usize],
        };
    }

    /// 获取当前CPU的TSS
    pub unsafe fn current_tss() -> &'static mut TaskStateSegment {
        &mut TSS_MANAGER.tss[smp_get_processor_id().data() as usize]
    }

    /// 加载当前CPU的TSS
    pub unsafe fn load_tr() {
        let index = (10 + smp_get_processor_id().data() * 2) as u16;
        let selector = SegmentSelector::new(index, Ring::Ring0);

        Self::set_tss_descriptor(
            index,
            VirtAddr::new(Self::current_tss() as *mut TaskStateSegment as usize),
        );
        x86::task::load_tr(selector);
    }

    unsafe fn set_tss_descriptor(index: u16, vaddr: VirtAddr) {
        const LIMIT: u64 = 103;
        let gdt_vaddr = VirtAddr::new(&GDT_Table as *const _ as usize);

        let gdt: &mut [u64] = core::slice::from_raw_parts_mut(gdt_vaddr.data() as *mut u64, 512);

        let vaddr = vaddr.data() as u64;
        gdt[index as usize] = (LIMIT & 0xffff)
            | ((vaddr & 0xffff) << 16)
            | (((vaddr >> 16) & 0xff) << 32)
            | (0x89 << 40)
            | (((vaddr >> 24) & 0xff) << 56);
        gdt[index as usize + 1] = ((vaddr >> 32) & 0xffffffff) | 0;
    }
}
