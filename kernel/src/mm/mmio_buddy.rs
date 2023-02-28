use crate::{
    arch::asm::current::{self, current_pcb},
    include::bindings::bindings::{
        initial_mm, mm_create_vma, mm_unmap, vm_area_del, vm_area_free, vm_area_struct, vm_flags_t,
        vma_find, EINVAL, ENOMEM, EPERM, MMIO_BASE, MMIO_TOP, PAGE_1G_SHIFT, PAGE_1G_SIZE,
        PAGE_2M_SIZE, PAGE_4K_SHIFT, PAGE_4K_SIZE, VM_DONTCOPY, VM_IO,
    },
    kdebug,
    libs::{
        mutex::MutexGuard,
        spinlock::{SpinLock, SpinLockGuard},
    },
};
use alloc::{boxed::Box, collections::LinkedList, vec::Vec};
use core::{mem, ptr::null_mut};

const MMIO_BUDDY_MAX_EXP: u32 = PAGE_1G_SHIFT;
const MMIO_BUDDY_MIN_EXP: u32 = PAGE_4K_SHIFT;
const MMIO_BUDDY_REGION_COUNT: u32 = MMIO_BUDDY_MAX_EXP - MMIO_BUDDY_MIN_EXP + 1;

pub enum MmioResult {
    SUCCESS,
    EINVAL,
}

///@brief buddy内存池
pub struct MmioBuddyMemPool {
    pool_start_addr: u64,
    pool_size: u64,
    free_regions: [SpinLock<MmioFreeRegionList>; MMIO_BUDDY_REGION_COUNT as usize],
}
impl Default for MmioBuddyMemPool {
    fn default() -> Self {
        MmioBuddyMemPool {
            pool_start_addr: Default::default(),
            pool_size: Default::default(),
            free_regions: unsafe { mem::zeroed() },
        }
    }
}
impl MmioBuddyMemPool {
    pub fn new() -> Self {
        return MmioBuddyMemPool {
            ..Default::default()
        };
    }

    ///@brief 创建新的地址区域结构体
    ///
    /// @param vaddr 虚拟地址
    ///
    /// @return 创建好的地址区域结构体
    pub fn __create_region(vaddr: u64) -> Box<MmioBuddyAddrRegion> {
        let mut region = Box::new(MmioBuddyAddrRegion::new());
        region.vaddr = vaddr;
        return region;
    }

    ///@brief 将内存块归还给buddy
    ///
    /// @param vaddr 虚拟地址
    ///
    /// @param exp 内存空间的大小（2^exp
    /// ）
    /// @param list_guard 【exp】对应的链表
    ///
    /// @return int 返回码
    pub fn __give_back_block(vaddr: u64, exp: u32) -> i32 {
        // 确保内存对齐，低位都要为0
        if (vaddr & ((1 << exp) - 1)) != 0 {
            return -(EINVAL as i32);
        }
        let region: Box<MmioBuddyAddrRegion> = Self::__create_region(vaddr);
        // 加入buddy
        let list_guard = &mut MMIO_POOL.free_regions[__exp2index(exp)].lock();
        Self::__buddy_add_region_obj(region, list_guard);
        return 0;
    }

    ///@brief 将给定大小为(2^exp)的地址空间一分为二，并插入内存块小一级的链表中
    ///
    /// @param region 要被分割的地址区域结构体（保证其已经从链表中取出）
    ///
    /// @param exp 要被分割的地址区域的大小的幂
    pub fn __buddy_split(
        region: Box<MmioBuddyAddrRegion>,
        exp: u32,
        list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) {
        let vaddr = Self::__buddy_block_vaddr(region.vaddr, exp - 1);
        let new_region = Self::__create_region(vaddr);
        // let mut list_guard = MMIO_POOL.free_regions[__exp2index(exp - 1) as usize].lock();
        Self::__buddy_add_region_obj(region, list_guard);
        Self::__buddy_add_region_obj(new_region, list_guard);
    }

    /// @brief 从buddy中申请一块指定大小的内存区域
    ///
    /// @param exp 内存区域的大小(2^exp)
    ///
    /// @param list_guard 上锁的exp对应的链表
    ///
    /// @return 符合要求的内存区域。没有满足要求的时候，返回MmioResult。
    pub fn __query_addr_region(
        exp: u32,
        list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) -> Result<Box<MmioBuddyAddrRegion>, MmioResult> {
        //申请范围错误
        if exp < MMIO_BUDDY_MIN_EXP || exp > MMIO_BUDDY_MAX_EXP {
            kdebug!("__query_addr_region: exp wrong");
            return Err(MmioResult::EINVAL);
        }
        //没有恰好符合要求的内存块
        //注意：exp对应的链表list_guard已上锁【注意避免死锁问题】
        if list_guard.num_free == 0 {
            //找到最小符合申请范围的内存块
            //将大的内存块依次分成小块内存，直到能够满足exp大小，即将exp+1分成两块exp
            for e in exp + 1..MMIO_BUDDY_MAX_EXP + 1 {
                if MMIO_POOL.free_regions[__exp2index(e) as usize]
                    .lock()
                    .num_free
                    == 0
                {
                    continue;
                }
                for e2 in (exp + 1..e + 1).rev() {
                    match Self::__buddy_pop_region(
                        &mut MMIO_POOL.free_regions[__exp2index(e2) as usize].lock(),
                    ) {
                        Ok(region) => {
                            if e2 != exp + 1 {
                                // 要将分裂后的内存块插入到更小的链表中
                                let low_list_guard = &mut MMIO_POOL.free_regions
                                    [__exp2index(e2 - 1) as usize]
                                    .lock();
                                Self::__buddy_split(region, e2, low_list_guard);
                            } else {
                                // 由于exp对应的链表list_guard已经被锁住了 不能再加锁
                                // 所以直接将list_guard传入
                                Self::__buddy_split(region, e2, list_guard);
                            }
                        }
                        Err(_) => {
                            kdebug!("buddy_pop_region get wrong");
                        }
                    }
                }
                break;
            }
            //判断是否获得了exp大小的内存块
            if list_guard.num_free > 0 {
                return Ok(list_guard.list.pop_back().unwrap());
            }
            //拆分大内存块无法获得exp大小内存块
            //尝试用小内存块合成
            //即将两块exp合成一块exp+1
            for e in MMIO_BUDDY_MIN_EXP..exp {
                if e != exp - 1 {
                    let high_list_guard = &mut MMIO_POOL.free_regions[__exp2index(exp + 1)].lock();
                    Self::__buddy_merge(
                        e,
                        &mut MMIO_POOL.free_regions[__exp2index(e) as usize].lock(),
                        high_list_guard,
                    );
                } else {
                    Self::__buddy_merge(
                        e,
                        &mut MMIO_POOL.free_regions[__exp2index(e) as usize].lock(),
                        list_guard,
                    );
                }
            }

            //判断是否获得了exp大小的内存块
            if list_guard.num_free > 0 {
                return Ok(list_guard.list.pop_back().unwrap());
            }
            return Err(MmioResult::EINVAL);
        } else {
            return Ok(list_guard.list.pop_back().unwrap());
        }
    }

    /// @brief 对query_addr_region进行封装
    ///
    /// @param exp 内存区域的大小(2^exp)
    ///
    /// @return 符合要求的内存区域。没有满足要求的时候，返回MmioResult。
    pub fn mmio_buddy_query_addr_region(exp: u32) -> Result<Box<MmioBuddyAddrRegion>, MmioResult> {
        let list_guard = &mut MMIO_POOL.free_regions[__exp2index(exp)].lock();
        match MmioBuddyMemPool::__query_addr_region(exp, list_guard) {
            Ok(ret) => return Ok(ret),
            Err(_) => {
                kdebug!("mmio_buddy_query_addr_region failed");
                return Err(MmioResult::EINVAL);
            }
        }
    }
    ///@brief 往指定的地址空间链表中添加一个地址区域
    ///
    ///@param region 要被添加的地址结构体
    ///
    ///@param list_guard 目标链表
    pub fn __buddy_add_region_obj(
        region: Box<MmioBuddyAddrRegion>,
        list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) {
        list_guard.list.push_back(region);
        list_guard.num_free += 1;
    }

    ///@brief 根据地址和内存块大小，计算伙伴块虚拟内存的地址
    pub fn __buddy_block_vaddr(vaddr: u64, exp: u32) -> u64 {
        return vaddr ^ (1 << exp);
    }

    /// @brief 寻找并弹出对应内存块的伙伴块
    ///
    /// @param region 对应内存块的信息
    ///
    /// @param exp 内存块大小
    ///
    /// @param list_guard 【exp】对应的链表
    ///
    /// @return 成功找到则返回伙伴块的引用，没找到则返回对应错误码
    pub fn __pop_buddy_block(
        vaddr: u64,
        exp: u32,
        list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) -> Result<Box<MmioBuddyAddrRegion>, MmioResult> {
        if list_guard.list.len() == 0 {
            return Err(MmioResult::EINVAL);
        } else {
            //计算伙伴块的地址
            let buddy_vaddr = Self::__buddy_block_vaddr(vaddr, exp);

            // element 只会有一个元素
            let mut element: Vec<Box<MmioBuddyAddrRegion>> = list_guard
                .list
                .drain_filter(|x| x.vaddr == buddy_vaddr)
                .collect();
            if element.len() == 1 {
                list_guard.num_free -= 1;
                return Ok(element.pop().unwrap());
            }

            //没有找到对应的伙伴块
            return Err(MmioResult::EINVAL);
        }
    }

    ///@brief 从空闲链表中取出指定大小的内存区域
    ///
    /// @param exp 内存大小的幂
    ///
    /// @param list_guard 【exp】对应的链表
    ///
    /// @return 成功时返回内存区域结构体的引用。失败时返回错误码。
    pub fn __buddy_pop_region(
        list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) -> Result<Box<MmioBuddyAddrRegion>, MmioResult> {
        if !list_guard.list.is_empty() {
            list_guard.num_free -= 1;
            return Ok(list_guard.list.pop_back().unwrap());
        }
        return Err(MmioResult::EINVAL);
    }

    /// @brief 合并exp对应链表上的块
    ///
    /// @param exp 内存块大小的幂(2^exp)
    ///
    /// @param list_guard exp对应的链表
    ///
    /// @param high_list_guard exp+1对应的链表
    pub fn __buddy_merge(
        exp: u32,
        list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
        high_list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) {
        // 至少要两个内存块才能合并
        if list_guard.num_free <= 1 {
            return;
        }
        loop {
            if list_guard.num_free <= 1 {
                break;
            }
            // 获取内存块
            let vaddr = list_guard.list.back().unwrap().vaddr;
            // 获取伙伴内存块
            match Self::__pop_buddy_block(vaddr, exp, list_guard) {
                Err(_) => continue,
                Ok(buddy_region) => {
                    let region = list_guard.list.pop_back().unwrap();
                    let copy_region: Box<MmioBuddyAddrRegion> = Box::new(MmioBuddyAddrRegion {
                        vaddr: region.vaddr,
                    });
                    // 在两块内存都被取出之后才进行合并
                    match Self::__buddy_merge_blocks(region, buddy_region, exp, high_list_guard) {
                        Err(_) => {
                            // 如果合并失败了要将取出来的元素放回去
                            MmioBuddyMemPool::__buddy_add_region_obj(copy_region, list_guard);
                            kdebug!("__buddy_merge get wrong");
                            return;
                        }
                        Ok(_) => continue,
                    }
                }
            }
        }
    }

    /// @brief 合并两个【已经从链表中取出】的伙伴块
    ///
    /// @param region_1 第一个伙伴块
    ///
    /// @param region_2 第二个伙伴块
    ///
    /// @return 错误码
    pub fn __buddy_merge_blocks(
        region_1: Box<MmioBuddyAddrRegion>,
        region_2: Box<MmioBuddyAddrRegion>,
        exp: u32,
        high_list_guard: &mut SpinLockGuard<MmioFreeRegionList>,
    ) -> Result<MmioResult, MmioResult> {
        // 判断是否为伙伴块
        if region_1.vaddr != Self::__buddy_block_vaddr(region_2.vaddr, exp) {
            return Err(MmioResult::EINVAL);
        }
        // FIXME q：被释放的region2是否已经将内存归还？
        // 将大的块放进下一级链表
        Self::__buddy_add_region_obj(region_1, high_list_guard);
        return Ok(MmioResult::SUCCESS);
    }
}

///@brief mmio伙伴系统内部的地址区域结构体
pub struct MmioBuddyAddrRegion {
    vaddr: u64,
}
impl MmioBuddyAddrRegion {
    pub fn new() -> Self {
        return MmioBuddyAddrRegion {
            ..Default::default()
        };
    }
}
impl Default for MmioBuddyAddrRegion {
    fn default() -> Self {
        MmioBuddyAddrRegion {
            vaddr: Default::default(),
        }
    }
}

///@brief 空闲页数组结构体
pub struct MmioFreeRegionList {
    //存储mmio_buddy的地址链表
    list: LinkedList<Box<MmioBuddyAddrRegion>>,
    //空闲块的数量
    num_free: i64,
}
impl MmioFreeRegionList {
    pub fn new() -> Self {
        return MmioFreeRegionList {
            ..Default::default()
        };
    }
}
impl Default for MmioFreeRegionList {
    fn default() -> Self {
        MmioFreeRegionList {
            list: Default::default(),
            num_free: 0,
        }
    }
}

lazy_static! {
    pub static ref MMIO_POOL: MmioBuddyMemPool = MmioBuddyMemPool::new();
}

///@brief 初始化mmio的伙伴系统
#[no_mangle]
pub extern "C" fn __mmio_buddy_init() {
    // 创建一堆1GB的地址块
    let cnt_1g_blocks: u32 = ((MMIO_TOP - MMIO_BASE) / PAGE_1G_SIZE as i64) as u32;
    let mut vaddr_base: u64 = MMIO_BASE as u64;
    for _ in 0..cnt_1g_blocks {
        let ret = MmioBuddyMemPool::__give_back_block(vaddr_base, PAGE_1G_SHIFT);
        if ret != 0 {
            kdebug!("__mmio_buddy_init failed");
            return;
        }
        vaddr_base += PAGE_1G_SIZE as u64;
    }
    kdebug!("__mmio_buddy_init success");
}

/// @brief 将内存对象大小的幂转换成内存池中的数组的下标
///
/// @param exp内存大小
///
/// @return 内存池数组下标
pub fn __exp2index(exp: u32) -> usize {
    return (exp - 12) as usize;
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
/// @return int 错误码
#[no_mangle]
pub extern "C" fn mmio_create(
    size: u32,
    vm_flags: vm_flags_t,
    res_vaddr: *mut u64,
    res_length: *mut u64,
) -> i32 {
    if size > PAGE_1G_SIZE || size == 0 {
        return -(EPERM as i32);
    }
    let mut retval = 0;
    // 计算前导0
    let mut size_exp: u32 = 31 - size.leading_zeros();
    // 记录最终申请的空间大小
    let mut new_size: u32 = size;
    // 对齐要申请的空间大小
    // 如果要申请的空间大小小于4k，则分配4k
    if size_exp < PAGE_4K_SHIFT {
        new_size = PAGE_4K_SIZE;
        size_exp = PAGE_4K_SHIFT;
    } else if (new_size & (!(1 << size_exp))) != 0 {
        // 向左对齐空间大小
        size_exp += 1;
        new_size = 1 << size_exp;
    }

    match MmioBuddyMemPool::mmio_buddy_query_addr_region(size_exp) {
        Ok(region) => {
            unsafe {
                *res_vaddr = region.vaddr;
                *res_length = new_size as u64;
            }
            //创建vma
            let flags = vm_flags | (VM_IO | VM_DONTCOPY) as u64;
            let len_4k: u64 = (new_size % PAGE_2M_SIZE) as u64;
            let len_2m: u64 = new_size as u64 - len_4k;
            let mut loop_i = 0;
            // 先分配2M的vma
            loop {
                if loop_i >= len_2m {
                    break;
                }
                retval = unsafe {
                    mm_create_vma(
                        &mut initial_mm,
                        region.vaddr + loop_i,
                        PAGE_2M_SIZE.into(),
                        flags,
                        null_mut(),
                        null_mut(),
                    )
                };
                if retval != 0 {
                    kdebug!(
                        "failed to create mmio 2m vma. pid = {:?}",
                        current_pcb().pid
                    );
                    // TODO: 当失败时，将已创建的vma删除
                    return retval;
                }
                loop_i += PAGE_2M_SIZE as u64;
            }
            //分配4K的vma
            loop_i = len_2m;
            loop {
                if loop_i >= size as u64 {
                    break;
                }
                retval = unsafe {
                    mm_create_vma(
                        &mut initial_mm,
                        region.vaddr + loop_i,
                        PAGE_4K_SIZE.into(),
                        flags,
                        null_mut(),
                        null_mut(),
                    )
                };
                if retval != 0 {
                    kdebug!(
                        "failed to create mmio 4k vma. pid = {:?}",
                        current_pcb().pid
                    );
                    // TODO: 当失败时，将已创建的vma删除
                    return retval;
                }
                loop_i += PAGE_4K_SIZE as u64;
            }
        }
        Err(_) => {
            kdebug!("failed to create mmio vma.pid = {:?}", current_pcb().pid);
            return -(ENOMEM as i32);
        }
    }
    return retval;
}

/// @brief 取消mmio的映射并将地址空间归还到buddy中
///
/// @param vaddr 起始的虚拟地址
///
/// @param length 要归还的地址空间的长度
///
/// @return int 错误码
#[no_mangle]
pub extern "C" fn mmio_release(vaddr: u64, length: u64) -> i32 {
    //先将要释放的空间取消映射
    unsafe {
        mm_unmap(&mut initial_mm, vaddr, length, false);
    }
    let mut retval = 0;
    let mut loop_i: u64 = 0;
    loop {
        if loop_i >= length {
            break;
        }
        // 获取要释放的vma的结构体
        let vma: *mut vm_area_struct = unsafe { vma_find(&mut initial_mm, vaddr + loop_i) };
        if vma == null_mut() {
            kdebug!(
                "mmio_release failed: vma not found. At address: {:?}, pid = {:?}",
                vaddr + loop_i,
                current_pcb().pid
            );
            return -(EINVAL as i32);
        }
        // 检查vma起始地址是否正确
        if unsafe { (*vma).vm_start != (vaddr + loop_i) } {
            kdebug!(
                "mmio_release failed: addr_start is not equal to current: {:?}. pid = {:?}",
                vaddr + loop_i,
                current_pcb().pid
            );
            return -(EINVAL as i32);
        }
        // 将vma对应空间归还
        retval = MmioBuddyMemPool::__give_back_block(unsafe { (*vma).vm_start }, unsafe {
            31 - ((*vma).vm_end - (*vma).vm_start).leading_zeros()
        });
        // 释放vma结构体
        unsafe {
            vm_area_del(vma);
            vm_area_free(vma);
        }

        if retval != 0 {
            kdebug!(
                "mmio_release give_back failed: pid = {:?}",
                current_pcb().pid
            );
            return retval;
        }
        loop_i += unsafe { (*vma).vm_end - (*vma).vm_start };
    }
    return retval;
}
