use system_error::SystemError;

use crate::{
    arch::MMArch,
    libs::{
        align::{page_align_down, page_align_up},
        spinlock::SpinLock,
    },
    mm::no_init::{pseudo_map_phys, pseudo_map_phys_ro, pseudo_unmap_phys},
};

use super::{allocator::page_frame::PageFrameCount, MemoryManagementArch, PhysAddr, VirtAddr};

static SLOTS: SpinLock<[Slot; EarlyIoRemap::SLOT_CNT]> =
    SpinLock::new([Slot::DEFAULT; EarlyIoRemap::SLOT_CNT]);

/// 早期IO映射机制
///
/// 该机制在内存管理初始化之前，提供IO重映射的功能。
///
/// ## 注意
///
/// 该机制使用固定大小的slot来记录所有的映射，
/// 而这些映射空间是有限的，由MMArch::FIXMAP_SIZE指定
pub struct EarlyIoRemap;

impl EarlyIoRemap {
    const SLOT_CNT: usize = MMArch::FIXMAP_SIZE / MMArch::PAGE_SIZE;

    /// 把物理内存映射到虚拟内存中（物理地址不要求对齐
    ///
    /// ## 参数
    ///
    /// - phys: 物理内存地址（不需要对齐）
    /// - size: 映射的内存大小
    /// - read_only: 映射区与是否只读
    ///
    /// ## 返回值
    ///
    /// - 成功： (phys对应的虚拟内存地址)
    /// - Err(SystemError::ENOMEM): 可用的slot不足
    #[allow(dead_code)]
    pub fn map_not_aligned(
        mut phys: PhysAddr,
        mut size: usize,
        read_only: bool,
    ) -> Result<VirtAddr, SystemError> {
        // debug!("map not aligned phys:{phys:?}, size:{size:?}, read_only:{read_only:?}");

        let offset = phys.data() - page_align_down(phys.data());
        size += offset;
        phys -= offset;

        let (map_vaddr, _) = Self::map(phys, size, read_only)?;
        return Ok(map_vaddr + offset);
    }

    /// 把物理内存映射到虚拟内存中
    ///
    /// ## 说明
    ///
    /// 虚拟内存由early io remap机制自动分配。
    ///
    /// ## 参数
    ///
    /// - phys: 物理内存地址（需要按页对齐）
    /// - size: 映射的内存大小
    /// - read_only: 映射区与是否只读
    ///
    /// ## 返回值
    ///
    /// - 成功： (虚拟内存地址, 映射的内存大小)
    /// - Err(SystemError::ENOMEM): 可用的slot不足
    /// - Err(SystemError::EINVAL): 传入的物理地址没有对齐
    #[allow(dead_code)]
    pub fn map(
        phys: PhysAddr,
        size: usize,
        read_only: bool,
    ) -> Result<(VirtAddr, usize), SystemError> {
        if !phys.check_aligned(MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }

        // debug!("Early io remap:{phys:?}, size:{size}");

        let mut slot_guard = SLOTS.lock();

        let slot_count = PageFrameCount::from_bytes(page_align_up(size))
            .unwrap()
            .data();
        // 寻找连续的slot
        let mut start_slot = None;
        for i in 0..(Self::SLOT_CNT - slot_count + 1) {
            let mut is_continuous = true;

            for j in 0..slot_count {
                let slot_idx = i + j;
                if slot_guard[slot_idx].start_idx.is_some() {
                    is_continuous = false;
                    break;
                }
            }

            if is_continuous {
                start_slot = Some(i);
                break;
            }
        }

        let start_slot = start_slot.ok_or(SystemError::ENOMEM)?;
        let vaddr = Self::idx_to_virt(start_slot);

        // debug!("start_slot:{start_slot}, vaddr: {vaddr:?}, slot_count: {slot_count:?}");
        let page_count = PageFrameCount::new(slot_count);
        // 执行映射
        if read_only {
            unsafe { pseudo_map_phys_ro(vaddr, phys, page_count) }
        } else {
            unsafe { pseudo_map_phys(vaddr, phys, page_count) }
        }

        // debug!("map ok");

        // 更新slot信息
        let map_size = slot_count * MMArch::PAGE_SIZE;
        for i in 0..slot_count {
            let slot_idx = start_slot + i;
            slot_guard[slot_idx].start_idx = Some(start_slot as u32);

            if i == 0 {
                slot_guard[slot_idx].size = map_size as u32;
                slot_guard[slot_idx].phys = phys;
            }
        }

        return Ok((vaddr, map_size));
    }

    /// 取消映射
    ///
    /// ## 参数
    ///
    /// - virt: 映射范围内的任意虚拟地址
    ///
    /// ## 返回值
    ///
    /// - Ok: 成功
    /// - Err(SystemError::EINVAL): 传入的虚拟地址不在early io remap范围内,
    ///     或者虚拟地址未映射
    #[allow(dead_code)]
    pub fn unmap(virt: VirtAddr) -> Result<(), SystemError> {
        if virt < MMArch::FIXMAP_START_VADDR || virt >= MMArch::FIXMAP_END_VADDR {
            return Err(SystemError::EINVAL);
        }

        let mut slot_guard = SLOTS.lock();
        let mut idx = None;

        // 寻找虚拟地址对应的区域的第一个slot
        for slot_idx in 0..Self::SLOT_CNT {
            let slot = &mut slot_guard[slot_idx];
            if let Some(start_idx) = slot.start_idx {
                if start_idx == slot_idx as u32 {
                    let vaddr_start = Self::idx_to_virt(start_idx as usize);
                    let vaddr_end = vaddr_start + slot.size as usize;
                    if vaddr_start <= virt && virt < vaddr_end {
                        // 找到区域了
                        idx = Some(slot_idx);
                        break;
                    }
                }
            }
        }

        let idx = idx.ok_or(SystemError::EINVAL)?;

        let vaddr = Self::idx_to_virt(idx);
        let count = PageFrameCount::from_bytes(slot_guard[idx].size as usize).unwrap();

        // 取消映射
        unsafe { pseudo_unmap_phys(vaddr, count) };

        for i in 0..count.data() {
            let slot_idx = idx + i;
            let slot = &mut slot_guard[slot_idx];
            *slot = Slot::DEFAULT;
        }

        return Ok(());
    }

    /// 把slot下标转换为这个slot对应的虚拟地址
    fn idx_to_virt(idx: usize) -> VirtAddr {
        MMArch::FIXMAP_START_VADDR + idx * MMArch::PAGE_SIZE
    }
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    /// 连续映射的起始槽位号
    start_idx: Option<u32>,
    /// 连续映射的区域大小（仅在起始槽位中设置）
    size: u32,
    /// 映射的起始物理地址
    phys: PhysAddr,
}

impl Slot {
    const DEFAULT: Self = Self {
        start_idx: None,
        size: 0,
        phys: PhysAddr::new(0),
    };
}
