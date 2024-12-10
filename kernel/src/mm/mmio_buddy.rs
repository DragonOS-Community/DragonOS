use crate::libs::align::{page_align_down, page_align_up};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::mm::kernel_mapper::KernelMapper;
use crate::mm::page::{PAGE_1G_SHIFT, PAGE_4K_SHIFT};
use crate::mm::{MMArch, MemoryManagementArch};
use crate::process::ProcessManager;

use alloc::{collections::LinkedList, vec::Vec};
use core::mem;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};
use log::{debug, error, info, warn};
use system_error::SystemError;

use super::page::{EntryFlags, PAGE_4K_SIZE};
use super::{PhysAddr, VirtAddr};

// 最大的伙伴块的幂
const MMIO_BUDDY_MAX_EXP: u32 = PAGE_1G_SHIFT as u32;
// 最小的伙伴块的幂
const MMIO_BUDDY_MIN_EXP: u32 = PAGE_4K_SHIFT as u32;
// 内存池数组的范围
const MMIO_BUDDY_REGION_COUNT: u32 = MMIO_BUDDY_MAX_EXP - MMIO_BUDDY_MIN_EXP + 1;

const PAGE_1G_SIZE: usize = 1 << 30;

static mut __MMIO_POOL: Option<MmioBuddyMemPool> = None;

pub fn mmio_pool() -> &'static MmioBuddyMemPool {
    unsafe { __MMIO_POOL.as_ref().unwrap() }
}

pub enum MmioResult {
    SUCCESS,
    EINVAL,
    ENOFOUND,
    WRONGEXP,
    ISEMPTY,
}

/// @brief buddy内存池
#[derive(Debug)]
pub struct MmioBuddyMemPool {
    pool_start_addr: VirtAddr,
    pool_size: usize,
    free_regions: [SpinLock<MmioFreeRegionList>; MMIO_BUDDY_REGION_COUNT as usize],
}

impl MmioBuddyMemPool {
    #[inline(never)]
    fn new() -> Self {
        let mut free_regions: [MaybeUninit<SpinLock<MmioFreeRegionList>>;
            MMIO_BUDDY_REGION_COUNT as usize] = unsafe { MaybeUninit::uninit().assume_init() };
        for i in 0..MMIO_BUDDY_REGION_COUNT {
            free_regions[i as usize] = MaybeUninit::new(SpinLock::new(MmioFreeRegionList::new()));
        }
        let free_regions = unsafe {
            mem::transmute::<
                [core::mem::MaybeUninit<
                    crate::libs::spinlock::SpinLock<crate::mm::mmio_buddy::MmioFreeRegionList>,
                >; MMIO_BUDDY_REGION_COUNT as usize],
                [SpinLock<MmioFreeRegionList>; MMIO_BUDDY_REGION_COUNT as usize],
            >(free_regions)
        };

        let pool = MmioBuddyMemPool {
            pool_start_addr: MMArch::MMIO_BASE,
            pool_size: MMArch::MMIO_SIZE,
            free_regions,
        };

        assert!(pool.pool_start_addr.data() % PAGE_1G_SIZE == 0);
        debug!("MMIO buddy pool init: created");

        let mut vaddr_base = MMArch::MMIO_BASE;
        let mut remain_size = MMArch::MMIO_SIZE;
        debug!(
            "BASE: {:?}, TOP: {:?}, size: {:?}",
            MMArch::MMIO_BASE,
            MMArch::MMIO_TOP,
            MMArch::MMIO_SIZE
        );

        for shift in (PAGE_4K_SHIFT..=PAGE_1G_SHIFT).rev() {
            if remain_size & (1 << shift) != 0 {
                let ok = pool.give_back_block(vaddr_base, shift as u32).is_ok();
                if ok {
                    vaddr_base += 1 << shift;
                    remain_size -= 1 << shift;
                } else {
                    panic!("MMIO buddy pool init failed");
                }
            }
        }

        debug!("MMIO buddy pool init success");
        return pool;
    }

    /// @brief 创建新的地址区域结构体
    ///
    /// @param vaddr 虚拟地址
    ///
    /// @return 创建好的地址区域结构体
    fn create_region(&self, vaddr: VirtAddr) -> MmioBuddyAddrRegion {
        // debug!("create_region for vaddr: {vaddr:?}");

        let region: MmioBuddyAddrRegion = MmioBuddyAddrRegion::new(vaddr);

        // debug!("create_region for vaddr: {vaddr:?} OK!!!");
        return region;
    }

    /// @brief 将内存块归还给buddy
    ///
    /// @param vaddr 虚拟地址
    ///
    /// @param exp 内存空间的大小（2^exp）
    ///
    /// @param list_guard 【exp】对应的链表
    ///
    /// @return Ok(i32) 返回0
    ///
    /// @return Err(SystemError) 返回错误码
    fn give_back_block(&self, vaddr: VirtAddr, exp: u32) -> Result<i32, SystemError> {
        // 确保内存对齐，低位都要为0
        if (vaddr.data() & ((1 << exp) - 1)) != 0 {
            return Err(SystemError::EINVAL);
        }
        let region: MmioBuddyAddrRegion = self.create_region(vaddr);
        // 加入buddy
        let mut list_guard = self.free_regions[exp2index(exp)].lock();

        self.push_block(region, &mut list_guard);
        return Ok(0);
    }

    /// @brief 将给定大小为2^{exp}的内存块一分为二，并插入内存块大小为2^{exp-1}的链表中
    ///
    /// @param region 要被分割的地址区域结构体（保证其已经从链表中取出）
    ///
    /// @param exp 要被分割的地址区域的大小的幂
    ///
    /// @param list_guard 【exp-1】对应的链表
    fn split_block(
        &self,
        region: MmioBuddyAddrRegion,
        exp: u32,
        low_list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) {
        let vaddr = self.calculate_block_vaddr(region.vaddr, exp - 1);
        let new_region: MmioBuddyAddrRegion = self.create_region(vaddr);
        self.push_block(region, low_list_guard);
        self.push_block(new_region, low_list_guard);
    }

    /// @brief 从buddy中申请一块指定大小的内存区域
    ///
    /// @param exp 要申请的内存块的大小的幂(2^exp)
    ///
    /// @param list_guard exp对应的链表
    ///
    /// @return Ok(MmioBuddyAddrRegion) 符合要求的内存区域。
    ///
    /// @return Err(MmioResult)
    /// - 没有满足要求的内存块时，返回ENOFOUND
    /// - 申请的内存块大小超过合法范围，返回WRONGEXP
    /// - 调用函数出错时，返回出错函数对应错误码
    fn query_addr_region(
        &self,
        exp: u32,
        list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) -> Result<MmioBuddyAddrRegion, MmioResult> {
        // 申请范围错误
        if !(MMIO_BUDDY_MIN_EXP..=MMIO_BUDDY_MAX_EXP).contains(&exp) {
            debug!("query_addr_region: exp wrong");
            return Err(MmioResult::WRONGEXP);
        }
        // 没有恰好符合要求的内存块
        // 注意：exp对应的链表list_guard已上锁【注意避免死锁问题】
        if list_guard.num_free == 0 {
            // 找到最小符合申请范围的内存块
            // 将大的内存块依次分成小块内存，直到能够满足exp大小，即将exp+1分成两块exp
            for e in exp + 1..MMIO_BUDDY_MAX_EXP + 1 {
                let pop_list: &mut SpinLockGuard<MmioFreeRegionList> =
                    &mut self.free_regions[exp2index(e)].lock();
                if pop_list.num_free == 0 {
                    continue;
                }

                for e2 in (exp + 1..e + 1).rev() {
                    if e2 == e {
                        match self.pop_block(pop_list) {
                            Ok(region) => {
                                if e2 != exp + 1 {
                                    // 要将分裂后的内存块插入到更小的链表中
                                    let low_list_guard: &mut SpinLockGuard<MmioFreeRegionList> =
                                        &mut self.free_regions[exp2index(e2 - 1)].lock();
                                    self.split_block(region, e2, low_list_guard);
                                } else {
                                    // 由于exp对应的链表list_guard已经被锁住了 不能再加锁
                                    // 所以直接将list_guard传入
                                    self.split_block(region, e2, list_guard);
                                }
                            }
                            Err(err) => {
                                debug!("buddy_pop_region get wrong");
                                return Err(err);
                            }
                        }
                    } else {
                        match self.pop_block(&mut self.free_regions[exp2index(e2)].lock()) {
                            Ok(region) => {
                                if e2 != exp + 1 {
                                    // 要将分裂后的内存块插入到更小的链表中
                                    let low_list_guard: &mut SpinLockGuard<MmioFreeRegionList> =
                                        &mut self.free_regions[exp2index(e2 - 1)].lock();
                                    self.split_block(region, e2, low_list_guard);
                                } else {
                                    // 由于exp对应的链表list_guard已经被锁住了 不能再加锁
                                    // 所以直接将list_guard传入
                                    self.split_block(region, e2, list_guard);
                                }
                            }
                            Err(err) => {
                                debug!("buddy_pop_region get wrong");
                                return Err(err);
                            }
                        }
                    }
                }
                break;
            }
            // 判断是否获得了exp大小的内存块
            if list_guard.num_free > 0 {
                match self.pop_block(list_guard) {
                    Ok(ret) => return Ok(ret),
                    Err(err) => return Err(err),
                }
            }
            // 拆分大内存块无法获得exp大小内存块
            // 尝试用小内存块合成
            // 即将两块exp合成一块exp+1

            // TODO：修改下一个循环的冗余代码，请不要删除此处的注释
            // let merge = |high_list_guard: &mut SpinLockGuard<MmioFreeRegionList>, exp: u32| {
            //     if let Err(err) = self.merge_all_exp(
            //         exp,
            //         &mut self.free_regions[exp2index(exp) as usize].lock(),
            //         high_list_guard,
            //     ) {
            //         return err;
            //     } else {
            //         return MmioResult::SUCCESS;
            //     }
            // };
            for e in MMIO_BUDDY_MIN_EXP..exp {
                if e != exp - 1 {
                    match self.merge_all_exp(
                        exp,
                        &mut self.free_regions[exp2index(exp)].lock(),
                        &mut self.free_regions[exp2index(exp + 1)].lock(),
                    ) {
                        Ok(_) => continue,
                        Err(err) => {
                            debug!("merge_all_exp get wrong");
                            return Err(err);
                        }
                    }
                } else {
                    match self.merge_all_exp(
                        exp,
                        &mut self.free_regions[exp2index(exp)].lock(),
                        list_guard,
                    ) {
                        Ok(_) => continue,
                        Err(err) => {
                            debug!("merge_all_exp get wrong");
                            return Err(err);
                        }
                    }
                }
            }

            //判断是否获得了exp大小的内存块
            if list_guard.num_free > 0 {
                match self.pop_block(list_guard) {
                    Ok(ret) => return Ok(ret),
                    Err(err) => return Err(err),
                }
            }
            return Err(MmioResult::ENOFOUND);
        } else {
            match self.pop_block(list_guard) {
                Ok(ret) => return Ok(ret),
                Err(err) => return Err(err),
            }
        }
    }

    /// @brief 对query_addr_region进行封装
    ///
    /// @param exp 内存区域的大小(2^exp)
    ///
    /// @return Ok(MmioBuddyAddrRegion)符合要求的内存块信息结构体。
    /// @return Err(MmioResult) 没有满足要求的内存块时，返回__query_addr_region的错误码。
    fn mmio_buddy_query_addr_region(&self, exp: u32) -> Result<MmioBuddyAddrRegion, MmioResult> {
        let mut list_guard: SpinLockGuard<MmioFreeRegionList> =
            self.free_regions[exp2index(exp)].lock();
        match self.query_addr_region(exp, &mut list_guard) {
            Ok(ret) => return Ok(ret),
            Err(err) => {
                debug!("mmio_buddy_query_addr_region failed");
                return Err(err);
            }
        }
    }
    /// @brief 往指定的地址空间链表中添加一个地址区域
    ///
    /// @param region 要被添加的地址结构体
    ///
    /// @param list_guard 目标链表
    fn push_block(
        &self,
        region: MmioBuddyAddrRegion,
        list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) {
        list_guard.list.push_back(region);
        list_guard.num_free += 1;
    }

    /// @brief 根据地址和内存块大小，计算伙伴块虚拟内存的地址
    #[inline(always)]
    fn calculate_block_vaddr(&self, vaddr: VirtAddr, exp: u32) -> VirtAddr {
        return VirtAddr::new(vaddr.data() ^ (1 << exp as usize));
    }

    /// @brief 寻找并弹出指定内存块的伙伴块
    ///
    /// @param region 对应内存块的信息
    ///
    /// @param exp 内存块大小
    ///
    /// @param list_guard 【exp】对应的链表
    ///
    /// @return Ok(Box<MmioBuddyAddrRegion) 返回伙伴块的引用
    /// @return Err(MmioResult)
    /// - 当链表为空，返回ISEMPTY
    /// - 没有找到伙伴块，返回ENOFOUND
    fn pop_buddy_block(
        &self,
        vaddr: VirtAddr,
        exp: u32,
        list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) -> Result<MmioBuddyAddrRegion, MmioResult> {
        if list_guard.list.is_empty() {
            return Err(MmioResult::ISEMPTY);
        } else {
            //计算伙伴块的地址
            let buddy_vaddr = self.calculate_block_vaddr(vaddr, exp);

            // element 只会有一个元素
            let mut element: Vec<MmioBuddyAddrRegion> = list_guard
                .list
                .extract_if(|x| x.vaddr == buddy_vaddr)
                .collect();
            if element.len() == 1 {
                list_guard.num_free -= 1;
                return Ok(element.pop().unwrap());
            }

            //没有找到对应的伙伴块
            return Err(MmioResult::ENOFOUND);
        }
    }

    /// @brief 从指定空闲链表中取出内存区域
    ///
    /// @param list_guard 【exp】对应的链表
    ///
    /// @return Ok(MmioBuddyAddrRegion) 内存块信息结构体的引用。
    ///
    /// @return Err(MmioResult) 当链表为空，无法删除时，返回ISEMPTY
    fn pop_block(
        &self,
        list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) -> Result<MmioBuddyAddrRegion, MmioResult> {
        if !list_guard.list.is_empty() {
            list_guard.num_free -= 1;
            return Ok(list_guard.list.pop_back().unwrap());
        }
        return Err(MmioResult::ISEMPTY);
    }

    /// @brief 合并所有2^{exp}大小的内存块
    ///
    /// @param exp 内存块大小的幂(2^exp)
    ///
    /// @param list_guard exp对应的链表
    ///
    /// @param high_list_guard exp+1对应的链表
    ///
    /// @return Ok(MmioResult) 合并成功返回SUCCESS
    /// @return Err(MmioResult)
    /// - 内存块过少，无法合并，返回EINVAL
    /// - pop_buddy_block调用出错，返回其错误码
    /// - merge_blocks调用出错，返回其错误码
    fn merge_all_exp(
        &self,
        exp: u32,
        list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
        high_list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) -> Result<MmioResult, MmioResult> {
        // 至少要两个内存块才能合并
        if list_guard.num_free <= 1 {
            return Err(MmioResult::EINVAL);
        }
        loop {
            if list_guard.num_free <= 1 {
                break;
            }
            // 获取内存块
            let vaddr: VirtAddr = list_guard.list.back().unwrap().vaddr;
            // 获取伙伴内存块
            match self.pop_buddy_block(vaddr, exp, list_guard) {
                Err(err) => {
                    return Err(err);
                }
                Ok(buddy_region) => {
                    let region: MmioBuddyAddrRegion = list_guard.list.pop_back().unwrap();
                    let copy_region = region.clone();
                    // 在两块内存都被取出之后才进行合并
                    match self.merge_blocks(region, buddy_region, exp, high_list_guard) {
                        Err(err) => {
                            // 如果合并失败了要将取出来的元素放回去
                            self.push_block(copy_region, list_guard);
                            debug!("merge_all_exp: merge_blocks failed");
                            return Err(err);
                        }
                        Ok(_) => continue,
                    }
                }
            }
        }
        return Ok(MmioResult::SUCCESS);
    }

    /// @brief 合并两个【已经从链表中取出】的内存块
    ///
    /// @param region_1 第一个内存块
    ///
    /// @param region_2 第二个内存
    ///
    /// @return Ok(MmioResult) 成功返回SUCCESS
    ///
    /// @return Err(MmioResult) 两个内存块不是伙伴块,返回EINVAL
    fn merge_blocks(
        &self,
        region_1: MmioBuddyAddrRegion,
        region_2: MmioBuddyAddrRegion,
        exp: u32,
        high_list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) -> Result<MmioResult, MmioResult> {
        // 判断是否为伙伴块
        if region_1.vaddr != self.calculate_block_vaddr(region_2.vaddr, exp) {
            return Err(MmioResult::EINVAL);
        }
        // 将大的块放进下一级链表
        self.push_block(region_1, high_list_guard);
        return Ok(MmioResult::SUCCESS);
    }

    /// @brief 创建一块mmio区域，并将vma绑定到initial_mm
    ///
    /// @param size mmio区域的大小（字节）
    ///
    /// @param vm_flags 要把vma设置成的标志
    ///
    /// @param res_vaddr 返回值-分配得到的虚拟地址
    ///
    /// @param res_length 返回值-分配的虚拟地址空间长度
    ///
    /// @return Ok(i32) 成功返回0
    ///
    /// @return Err(SystemError) 失败返回错误码
    pub fn create_mmio(&self, size: usize) -> Result<MMIOSpaceGuard, SystemError> {
        if size > PAGE_1G_SIZE || size == 0 {
            return Err(SystemError::EPERM);
        }
        // 计算前导0
        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
        let mut size_exp: u32 = 63 - size.leading_zeros();
        // debug!("create_mmio: size_exp: {}", size_exp);
        // 记录最终申请的空间大小
        let mut new_size = size;
        // 对齐要申请的空间大小
        // 如果要申请的空间大小小于4k，则分配4k
        if size_exp < PAGE_4K_SHIFT as u32 {
            new_size = PAGE_4K_SIZE;
            size_exp = PAGE_4K_SHIFT as u32;
        } else if (new_size & (!(1 << size_exp))) != 0 {
            // 向左对齐空间大小
            size_exp += 1;
            new_size = 1 << size_exp;
        }
        match self.mmio_buddy_query_addr_region(size_exp) {
            Ok(region) => {
                let space_guard =
                    unsafe { MMIOSpaceGuard::from_raw(region.vaddr, new_size, false) };
                return Ok(space_guard);
            }
            Err(_) => {
                error!(
                    "failed to create mmio. pid = {:?}",
                    ProcessManager::current_pcb().pid()
                );
                return Err(SystemError::ENOMEM);
            }
        }
    }

    /// @brief 取消mmio的映射并将地址空间归还到buddy中
    ///
    /// @param vaddr 起始的虚拟地址
    ///
    /// @param length 要归还的地址空间的长度
    ///
    /// @return Ok(i32) 成功返回0
    ///
    /// @return Err(SystemError) 失败返回错误码
    pub fn release_mmio(&self, vaddr: VirtAddr, length: usize) -> Result<i32, SystemError> {
        assert!(vaddr.check_aligned(MMArch::PAGE_SIZE));
        assert!(length & (MMArch::PAGE_SIZE - 1) == 0);
        if vaddr < self.pool_start_addr
            || vaddr.data() >= self.pool_start_addr.data() + self.pool_size
        {
            return Err(SystemError::EINVAL);
        }
        // todo: 重构MMIO管理机制，创建类似全局的manager之类的，管理MMIO的空间？

        // 暂时认为传入的vaddr都是正确的
        let page_count = length / MMArch::PAGE_SIZE;
        // 取消映射
        let mut bindings = KernelMapper::lock();
        let mut kernel_mapper = bindings.as_mut();
        if kernel_mapper.is_none() {
            warn!("release_mmio: kernel_mapper is read only");
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        for i in 0..page_count {
            unsafe {
                let x: Option<(
                    PhysAddr,
                    EntryFlags<MMArch>,
                    crate::mm::page::PageFlush<MMArch>,
                )> = kernel_mapper
                    .as_mut()
                    .unwrap()
                    .unmap_phys(vaddr + i * MMArch::PAGE_SIZE, false);
                if let Some((_, _, flush)) = x {
                    flush.flush();
                }
            };
        }

        // 归还到buddy
        mmio_pool()
            .give_back_block(vaddr, length.trailing_zeros())
            .unwrap_or_else(|err| {
                panic!("MMIO release failed: self: {self:?}, err msg: {:?}", err);
            });

        return Ok(0);
    }
}

/// @brief mmio伙伴系统内部的地址区域结构体
#[derive(Debug, Clone)]
struct MmioBuddyAddrRegion {
    vaddr: VirtAddr,
}
impl MmioBuddyAddrRegion {
    pub fn new(vaddr: VirtAddr) -> Self {
        return MmioBuddyAddrRegion { vaddr };
    }

    #[allow(dead_code)]
    pub fn vaddr(&self) -> VirtAddr {
        return self.vaddr;
    }
}

/// @brief 空闲页数组结构体
#[derive(Debug, Default)]
pub struct MmioFreeRegionList {
    /// 存储mmio_buddy的地址链表
    list: LinkedList<MmioBuddyAddrRegion>,
    /// 空闲块的数量
    num_free: i64,
}
impl MmioFreeRegionList {
    #[allow(dead_code)]
    fn new() -> Self {
        return MmioFreeRegionList {
            ..Default::default()
        };
    }
}

/// @brief 将内存对象大小的幂转换成内存池中的数组的下标
///
/// @param exp内存大小
///
/// @return 内存池数组下标
#[inline(always)]
fn exp2index(exp: u32) -> usize {
    return (exp - 12) as usize;
}

#[derive(Debug)]
pub struct MMIOSpaceGuard {
    vaddr: VirtAddr,
    size: usize,
    mapped: AtomicBool,
}

impl MMIOSpaceGuard {
    pub unsafe fn from_raw(vaddr: VirtAddr, size: usize, mapped: bool) -> Self {
        // check size
        assert!(
            size & (MMArch::PAGE_SIZE - 1) == 0,
            "MMIO space size must be page aligned"
        );
        assert!(size.is_power_of_two(), "MMIO space size must be power of 2");
        assert!(
            vaddr.check_aligned(size),
            "MMIO space vaddr must be aligned with size"
        );
        assert!(
            vaddr.data() >= MMArch::MMIO_BASE.data()
                && vaddr.data() + size <= MMArch::MMIO_TOP.data(),
            "MMIO space must be in MMIO region"
        );

        // 人工创建的MMIO空间，认为已经映射
        MMIOSpaceGuard {
            vaddr,
            size,
            mapped: AtomicBool::new(mapped),
        }
    }

    pub fn vaddr(&self) -> VirtAddr {
        self.vaddr
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// 将物理地址填写到虚拟地址空间中
    ///
    /// ## Safety
    ///
    /// 传入的物理地址【一定要是设备的物理地址】。
    /// 如果物理地址是从内存分配器中分配的，那么会造成内存泄露。因为mmio_release的时候，只取消映射，不会释放内存。
    pub unsafe fn map_phys(&self, paddr: PhysAddr, length: usize) -> Result<(), SystemError> {
        if length > self.size {
            return Err(SystemError::EINVAL);
        }

        let check = self
            .mapped
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst);
        if check.is_err() {
            return Err(SystemError::EINVAL);
        }

        let flags = EntryFlags::mmio_flags();

        let mut kernel_mapper = KernelMapper::lock();
        let r = kernel_mapper.map_phys_with_size(self.vaddr, paddr, length, flags, true);
        return r;
    }

    /// 将物理地址填写到虚拟地址空间中
    ///
    /// ## Safety
    ///
    /// 传入的物理地址【一定要是设备的物理地址】。
    /// 如果物理地址是从内存分配器中分配的，那么会造成内存泄露。因为mmio_release的时候，只取消映射，不会释放内存。
    pub unsafe fn map_phys_with_flags(
        &self,
        paddr: PhysAddr,
        length: usize,
        flags: EntryFlags<MMArch>,
    ) -> Result<(), SystemError> {
        if length > self.size {
            return Err(SystemError::EINVAL);
        }

        let check = self
            .mapped
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst);
        if check.is_err() {
            return Err(SystemError::EINVAL);
        }

        let mut kernel_mapper = KernelMapper::lock();
        let r = kernel_mapper.map_phys_with_size(self.vaddr, paddr, length, flags, true);
        return r;
    }

    /// # map_any_phys - 将任意物理地址映射到虚拟地址
    ///
    /// 将指定的物理地址和长度映射到虚拟地址空间。
    ///
    /// ## 参数
    ///
    /// - `paddr`: 物理地址，需要被映射的起始地址。
    /// - `length`: 要映射的物理地址长度。
    ///
    /// ## 返回值
    /// - `Ok(VirtAddr)`: 映射成功，返回虚拟地址的起始地址。
    /// - `Err(SystemError)`: 映射失败，返回系统错误。
    ///
    /// ## 副作用
    ///
    /// 该函数会修改虚拟地址空间，将物理地址映射到虚拟地址。
    ///
    /// ## Safety
    ///
    /// 由于该函数涉及到内存操作，因此它是非安全的。确保在调用该函数时，你传入的物理地址是正确的。
    #[allow(dead_code)]
    pub unsafe fn map_any_phys(
        &self,
        paddr: PhysAddr,
        length: usize,
    ) -> Result<VirtAddr, SystemError> {
        let paddr_base = PhysAddr::new(page_align_down(paddr.data()));
        let offset = paddr - paddr_base;
        let vaddr_base = self.vaddr;
        let vaddr = vaddr_base + offset;

        self.map_phys(paddr_base, page_align_up(length + offset))?;
        return Ok(vaddr);
    }

    /// 泄露一个MMIO space guard，不会释放映射的空间
    pub unsafe fn leak(self) {
        core::mem::forget(self);
    }
}

impl Drop for MMIOSpaceGuard {
    fn drop(&mut self) {
        let _ = mmio_pool()
            .release_mmio(self.vaddr, self.size)
            .unwrap_or_else(|err| {
                panic!("MMIO release failed: self: {self:?}, err msg: {:?}", err);
            });
    }
}

pub fn mmio_init() {
    debug!("Initializing MMIO buddy memory pool...");
    // 初始化mmio内存池
    unsafe {
        __MMIO_POOL = Some(MmioBuddyMemPool::new());
    }

    info!("MMIO buddy memory pool init done");
}
