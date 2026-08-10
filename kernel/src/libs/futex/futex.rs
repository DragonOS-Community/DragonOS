use alloc::{
    boxed::Box,
    collections::LinkedList,
    sync::{Arc, Weak},
};
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
))]
use core::arch::asm;
use core::{
    hash::{Hash, Hasher},
    intrinsics::{likely, unlikely},
    mem,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU32, Ordering},
};
use log::warn;

use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    arch::MMArch,
    libs::{
        mutex::{Mutex, MutexGuard},
        wait_queue::{Waiter, Waker},
    },
    mm::{
        access_ok,
        fault::{FaultFlags, PageFaultHandler, PageFaultMessage},
        ucontext::AddressSpace,
        MemoryManagementArch, VirtAddr, VirtRegion, VmFaultReason, VmFlags,
    },
    process::{preempt::PageFaultDisabledGuard, ProcessControlBlock, ProcessManager, RawPid},
    syscall::user_access::{UserBufferReader, UserBufferWriter},
    time::{
        timer::{next_n_us_timer_jiffies, Timer, TimerFunction},
        PosixTimeSpec,
    },
};

use super::constant::*;

static mut FUTEX_DATA: Option<FutexData> = None;

pub struct FutexData {
    data: Mutex<HashMap<FutexKey, FutexHashBucket>>,
}

impl FutexData {
    pub fn futex_map() -> MutexGuard<'static, HashMap<FutexKey, FutexHashBucket>> {
        unsafe { FUTEX_DATA.as_ref().unwrap().data.lock() }
    }

    pub fn try_remove(key: &FutexKey) -> Option<FutexHashBucket> {
        unsafe {
            let mut guard = FUTEX_DATA.as_ref().unwrap().data.lock();
            if let Some(futex) = guard.get(key) {
                if futex.chain.is_empty() && futex.pi_waiters.is_empty() && futex.pi_owner == 0 {
                    return guard.remove(key);
                }
            }
        }
        None
    }
}

pub struct Futex;

// 对于同一个futex的进程或线程将会在这个bucket等待
pub struct FutexHashBucket {
    // 该futex维护的等待队列
    pub(super) chain: LinkedList<Arc<FutexObj>>,
    // PI futex state
    pub(super) pi_owner: u32,
    pub(super) pi_waiters: LinkedList<Arc<FutexObj>>,
}

impl FutexHashBucket {
    pub fn new() -> Self {
        Self {
            chain: LinkedList::new(),
            pi_owner: 0,
            pi_waiters: LinkedList::new(),
        }
    }

    /// ## 判断是否在bucket里
    #[allow(dead_code)]
    pub fn contains(&self, futex_q: &FutexObj) -> bool {
        self.chain
            .iter()
            .filter(|x| Arc::ptr_eq(&x.waker, &futex_q.waker) && x.key == futex_q.key)
            .count()
            != 0
    }

    #[inline(always)]
    pub fn enqueue(&mut self, futex_q: Arc<FutexObj>) {
        self.chain.push_back(futex_q);
    }

    /// ## 唤醒队列中的最多nr_wake个进程
    ///
    /// return: 唤醒的进程数
    pub fn wake_up(
        &mut self,
        key: FutexKey,
        bitset: Option<u32>,
        nr_wake: u32,
    ) -> Result<usize, SystemError> {
        let mut count = 0;
        // 记录初始队列长度，确保只遍历一次
        let initial_len = self.chain.len();
        let mut processed = 0;

        while processed < initial_len && count < nr_wake {
            if let Some(futex_q) = self.chain.pop_front() {
                // 检查key是否匹配
                if futex_q.key != key {
                    // key不匹配，放回队列尾部
                    self.chain.push_back(futex_q);
                    processed += 1;
                    continue;
                }

                // 检查bitset是否匹配
                if let Some(bitset) = bitset {
                    if futex_q.bitset & bitset == 0 {
                        // bitset不匹配，放回队列尾部
                        self.chain.push_back(futex_q);
                        processed += 1;
                        continue;
                    }
                }

                // key和bitset都匹配，尝试唤醒
                // 注意：pop_front已经将futex_q从队列中移除，无需再次调用remove
                if futex_q.waker.wake() {
                    count += 1;
                }
                // 如果pcb已经被释放，也算处理了一个，继续下一个
                processed += 1;
            } else {
                // 队列为空，退出
                break;
            }
        }

        Ok(count as usize)
    }

    /// 将FutexObj从bucket中删除
    pub fn remove_by_waker(&mut self, waker: &Arc<Waker>) -> bool {
        let before = self.chain.len();
        self.chain
            .extract_if(|x| Arc::ptr_eq(&x.waker, waker))
            .for_each(drop);
        before != self.chain.len()
    }
}

#[derive(Debug)]
pub struct FutexObj {
    pub(super) waker: Arc<Waker>,
    pub(super) key: FutexKey,
    pub(super) bitset: u32,
    pub(super) tid: u32,
    // TODO: 优先级继承
}

#[derive(Debug)]
struct WakerTimer {
    waker: Arc<Waker>,
}

impl TimerFunction for WakerTimer {
    fn run(&mut self) -> Result<(), SystemError> {
        self.waker.wake();
        Ok(())
    }
}

pub enum FutexAccess {
    FutexRead,
    FutexWrite,
}

#[allow(dead_code)]
#[derive(Hash, PartialEq, Eq, Clone, Debug)]
/// ### 用于定位内核唯一的futex
pub enum InnerFutexKey {
    Shared(SharedKey),
    Private(PrivateKey),
}

#[derive(Hash, PartialEq, Eq, Clone, Debug)]
pub struct FutexKey {
    ptr: u64,
    word: u64,
    offset: u32,
    key: InnerFutexKey,
}

/// 共享 futex 的类型
#[derive(Hash, PartialEq, Eq, Clone, Debug)]
pub enum SharedKeyKind {
    /// 文件映射的 futex
    File { dev: u64, ino: u64 },
    /// 显式共享的匿名映射（MAP_SHARED | MAP_ANONYMOUS）
    SharedAnon { id: u64 },
    /// 私有匿名映射上的 FUTEX_SHARED（栈、堆等）
    /// 只能在同一进程的线程间同步
    PrivateAnonShared { as_id: u64 },
}

/// 不同进程间通过文件或共享内存共享futex变量
#[derive(Hash, PartialEq, Eq, Clone, Debug)]
pub struct SharedKey {
    kind: SharedKeyKind,
    page_offset: u64,
}

/// 同一进程的不同线程共享futex变量，表明该变量在进程地址空间中的位置
#[derive(Clone, Debug)]
pub struct PrivateKey {
    // 所在的地址空间
    address_space: Option<Weak<AddressSpace>>,
    // 表示所在页面的初始地址
    address: u64,
}

impl Hash for PrivateKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.address.hash(state);
    }
}

impl Eq for PrivateKey {}

impl PartialEq for PrivateKey {
    fn eq(&self, other: &Self) -> bool {
        if self.address_space.is_none() && other.address_space.is_none() {
            return self.address == other.address;
        } else {
            return self
                .address_space
                .as_ref()
                .unwrap_or(&Weak::default())
                .ptr_eq(other.address_space.as_ref().unwrap_or(&Weak::default()))
                && self.address == other.address;
        }
    }
}

impl Futex {
    /// ### 初始化FUTEX_DATA
    pub fn init() {
        unsafe {
            FUTEX_DATA = Some(FutexData {
                data: Mutex::new(HashMap::new()),
            })
        };
    }

    fn fault_in_user_writeable_u32(uaddr: VirtAddr) -> Result<(), SystemError> {
        access_ok(uaddr, core::mem::size_of::<u32>()).map_err(|_| SystemError::EFAULT)?;

        let fault_region = VirtRegion::new(
            VirtAddr::new(uaddr.data() & !MMArch::PAGE_OFFSET_MASK),
            MMArch::PAGE_SIZE,
        );
        let current_address_space = AddressSpace::current()?;
        let mut flags = FaultFlags::FAULT_FLAG_WRITE;

        loop {
            let mut space_guard =
                current_address_space.write_guard_no_reservation_conflict(fault_region);
            let vma = space_guard
                .mappings
                .find_nearest(uaddr)
                .ok_or(SystemError::EFAULT)?;

            let accessible = vma.is_accessible();
            let (region, vm_flags) = {
                let guard = vma.lock();
                (*guard.region(), *guard.vm_flags())
            };
            if !region.contains(uaddr) || !accessible || !vm_flags.contains(VmFlags::VM_WRITE) {
                return Err(SystemError::EFAULT);
            }

            let mapper = &mut space_guard.user_mapper.utable;
            let fault = unsafe {
                let message =
                    PageFaultMessage::new(vma, uaddr, flags, mapper, current_address_space.clone());
                PageFaultHandler::handle_mm_fault(message)
            };

            if fault.reason.contains(VmFaultReason::VM_FAULT_COMPLETED) {
                return Ok(());
            }
            if fault.reason.contains(VmFaultReason::VM_FAULT_RETRY) {
                flags |= FaultFlags::FAULT_FLAG_TRIED;
                let wait = fault.retry_wait;
                drop(space_guard);
                if let Some(wait) = wait {
                    wait.wait().map_err(|_| SystemError::EFAULT)?;
                }
                continue;
            }
            return Err(SystemError::EFAULT);
        }
    }

    /// ### 让当前进程在指定futex上等待直到futex_wake显式唤醒
    pub fn futex_wait(
        uaddr: VirtAddr,
        flags: FutexFlag,
        val: u32,
        abs_time: Option<PosixTimeSpec>,
        bitset: u32,
    ) -> Result<usize, SystemError> {
        if bitset == 0 {
            return Err(SystemError::EINVAL);
        }

        // 获取全局hash表的key值
        let key = Self::get_futex_key(
            uaddr,
            flags.contains(FutexFlag::FLAGS_SHARED),
            FutexAccess::FutexRead,
        )?;

        let mut futex_map_guard = FutexData::futex_map();
        let bucket = futex_map_guard.get_mut(&key);
        let bucket_mut = match bucket {
            Some(bucket) => bucket,
            None => {
                let bucket = FutexHashBucket::new();
                futex_map_guard.insert(key.clone(), bucket);
                futex_map_guard.get_mut(&key).unwrap()
            }
        };

        // 使用UserBuffer读取futex
        let user_reader =
            UserBufferReader::new(uaddr.as_ptr::<u32>(), core::mem::size_of::<u32>(), true)?;

        // 从用户空间读取到futex的val
        let mut uval = 0;

        // 读取
        // 这里只尝试一种方式去读取用户空间，与linux不太一致
        // 对于linux，如果bucket被锁住时读取失败，将会将bucket解锁后重新读取
        user_reader.copy_one_from_user::<u32>(&mut uval, 0)?;

        // 不满足wait条件，返回错误
        if uval != val {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let (waiter, waker) = Waiter::new_pair();
        // 创建超时计时器任务
        let mut timer = None;
        if let Some(time) = abs_time {
            if !time.is_valid_timeout() {
                return Err(SystemError::EINVAL);
            }
            let total_us = time.to_ktime_ns().div_ceil(1000);

            // 如果超时时间为0，直接返回ETIMEDOUT
            if total_us == 0 {
                return Err(SystemError::ETIMEDOUT);
            }

            let jiffies = next_n_us_timer_jiffies(total_us);
            let wake_up = Timer::new(
                Box::new(WakerTimer {
                    waker: waker.clone(),
                }),
                jiffies,
            );
            timer = Some(wake_up);
        }

        let futex_q = Arc::new(FutexObj {
            waker: waker.clone(),
            key: key.clone(),
            bitset,
            tid: 0,
        });
        bucket_mut.enqueue(futex_q.clone());

        // 在入队后激活定时器，避免短超时在阻塞之前触发造成唤醒丢失
        if let Some(ref t) = timer {
            t.activate();
        }

        drop(futex_map_guard);
        // 阻塞等待
        let wait_res = waiter.wait(true);

        // ========== 被唤醒后的检查 ==========
        // 进程被唤醒可能有以下几种情况：
        // 1. futex_wake 显式唤醒（正常情况）- futex_q 已从队列移除
        // 2. 超时唤醒 - futex_q 仍在队列中
        // 3. 信号唤醒 - futex_q 仍在队列中
        // 4. 伪唤醒 - futex_q 仍在队列中

        let mut futex_map_guard = FutexData::futex_map();

        // 首先检查超时
        let is_timeout = timer.as_ref().is_some_and(|t| t.timeout());

        // 从队列中移除自身（如果仍在队列）
        let in_queue = futex_map_guard
            .get_mut(&key)
            .map(|bucket| bucket.remove_by_waker(&waker))
            .unwrap_or(false);

        drop(futex_map_guard);

        // 取消定时器
        if let Some(timer) = timer {
            timer.cancel();
        }

        // 处理等待结果
        if is_timeout {
            return Err(SystemError::ETIMEDOUT);
        }

        if wait_res.is_err() || ProcessManager::current_pcb().has_pending_signal() {
            // 按 Linux 语义，将可中断唤醒视为 EINTR 而非内部重启错误
            return Err(SystemError::EINTR);
        }

        if in_queue {
            // 未被正常唤醒，视为伪唤醒/信号
            return Err(SystemError::EINTR);
        }

        Ok(0)
    }

    // ### 唤醒指定futex上挂起的最多nr_wake个进程
    ///
    /// ### Linux 语义
    /// 根据 Linux 的实际行为，即使 nr_wake 为 0，FUTEX_WAKE 也会唤醒至少一个等待者。
    /// 这是 FUTEX_WAKE 特有的行为，其他操作如 FUTEX_REQUEUE 不适用此规则。
    pub fn futex_wake(
        uaddr: VirtAddr,
        flags: FutexFlag,
        nr_wake: u32,
        bitset: u32,
    ) -> Result<usize, SystemError> {
        if bitset == 0 {
            return Err(SystemError::EINVAL);
        }

        // 获取futex_key,并且判断地址空间合法性
        let key = Self::get_futex_key(
            uaddr,
            flags.contains(FutexFlag::FLAGS_SHARED),
            FutexAccess::FutexRead,
        )?;

        let mut binding = FutexData::futex_map();
        let bucket_mut = binding.entry(key.clone()).or_insert(FutexHashBucket::new());

        // 确保后面的唤醒操作是有意义的
        if bucket_mut.chain.is_empty() {
            return Ok(0);
        }

        // Linux 行为：即使 nr_wake 为 0，也至少唤醒一个等待者
        let effective_nr_wake = if nr_wake == 0 { 1 } else { nr_wake };

        // 从队列中唤醒
        let count = bucket_mut.wake_up(key.clone(), Some(bitset), effective_nr_wake)?;

        drop(binding);

        FutexData::try_remove(&key);

        Ok(count)
    }

    /// ### 唤醒制定uaddr1上的最多nr_wake个进程，然后将uaddr1最多nr_requeue个进程移动到uaddr2绑定的futex上
    pub fn futex_requeue(
        uaddr1: VirtAddr,
        flags: FutexFlag,
        uaddr2: VirtAddr,
        nr_wake: i32,
        nr_requeue: i32,
        cmpval: Option<u32>,
        requeue_pi: bool,
    ) -> Result<usize, SystemError> {
        if nr_requeue < 0 || nr_wake < 0 {
            return Err(SystemError::EINVAL);
        }

        // 暂时不支持优先级继承
        if requeue_pi {
            return Err(SystemError::ENOSYS);
        }

        let key1 = Self::get_futex_key(
            uaddr1,
            flags.contains(FutexFlag::FLAGS_SHARED),
            FutexAccess::FutexRead,
        )?;
        let key2 = Self::get_futex_key(uaddr2, flags.contains(FutexFlag::FLAGS_SHARED), {
            match requeue_pi {
                true => FutexAccess::FutexWrite,
                false => FutexAccess::FutexRead,
            }
        })?;

        if requeue_pi && key1 == key2 {
            return Err(SystemError::EINVAL);
        }

        if likely(cmpval.is_some()) {
            let uval_reader =
                UserBufferReader::new(uaddr1.as_ptr::<u32>(), core::mem::size_of::<u32>(), true)?;
            let curval = uval_reader.read_one_from_user::<u32>(0)?;

            // 判断是否满足条件
            if curval != cmpval.unwrap() {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        }

        let mut futex_data_guard = FutexData::futex_map();
        if !requeue_pi {
            // 唤醒nr_wake个进程
            let bucket_1_mut = futex_data_guard.get_mut(&key1).ok_or(SystemError::EINVAL)?;
            let ret = bucket_1_mut.wake_up(key1.clone(), None, nr_wake as u32)?;
            // 将bucket1中最多nr_requeue个任务转移到bucket2
            for _ in 0..nr_requeue {
                let bucket_1_mut = futex_data_guard.get_mut(&key1).ok_or(SystemError::EINVAL)?;
                let futex_q = bucket_1_mut.chain.pop_front();
                match futex_q {
                    Some(futex_q) => {
                        let bucket_2_mut =
                            futex_data_guard.get_mut(&key2).ok_or(SystemError::EINVAL)?;
                        bucket_2_mut.chain.push_back(futex_q);
                    }
                    None => {
                        break;
                    }
                }
            }

            return Ok(ret);
        } else {
            // 暂时不支持优先级继承
            todo!()
        }
    }

    /// ### 唤醒futex上的进程的同时进行一些操作
    pub fn futex_wake_op(
        uaddr1: VirtAddr,
        flags: FutexFlag,
        uaddr2: VirtAddr,
        nr_wake: i32,
        nr_wake2: i32,
        op: i32,
    ) -> Result<usize, SystemError> {
        loop {
            // Linux semantics: For private futex, allow uaddr1 to be NULL; in that case only execute the op without waking any waiters from uaddr1.
            let key1 = Futex::get_futex_key(
                uaddr1,
                flags.contains(FutexFlag::FLAGS_SHARED),
                FutexAccess::FutexRead,
            )?;
            let key2 = Futex::get_futex_key(
                uaddr2,
                flags.contains(FutexFlag::FLAGS_SHARED),
                FutexAccess::FutexWrite,
            )?;

            let mut futex_data_guard = FutexData::futex_map();
            let op_ret = match Self::futex_atomic_op_inuser(op as u32, uaddr2) {
                Ok(ret) => ret,
                Err(SystemError::EFAULT) => {
                    drop(futex_data_guard);
                    Self::fault_in_user_writeable_u32(uaddr2)?;
                    continue;
                }
                Err(e) => return Err(e),
            };

            let mut wake_count = 0;
            let nr_wake = if nr_wake <= 0 { 1 } else { nr_wake as u32 };
            let nr_wake2 = if nr_wake2 <= 0 { 1 } else { nr_wake2 as u32 };
            // If uaddr1 has no associated waiters, return 0 instead of EINVAL per Linux behavior.
            if let Some(bucket1) = futex_data_guard.get_mut(&key1) {
                // Wake up processes in uaddr1
                wake_count += bucket1.wake_up(key1.clone(), None, nr_wake)?;
            }

            // If the operation succeeds, wake up processes in uaddr2
            if op_ret {
                // If uaddr2 has no associated waiters, skip the wake instead of returning EINVAL per Linux behavior.
                if let Some(bucket2) = futex_data_guard.get_mut(&key2) {
                    wake_count += bucket2.wake_up(key2, None, nr_wake2)?;
                }
            }

            return Ok(wake_count);
        }
    }

    pub(super) fn get_futex_key(
        uaddr: VirtAddr,
        fshared: bool,
        _access: FutexAccess,
    ) -> Result<FutexKey, SystemError> {
        let mut address = uaddr.data();

        // 计算相对页的偏移量
        let offset = address & (MMArch::PAGE_SIZE - 1);
        // 判断内存对齐
        if uaddr.data() & (core::mem::size_of::<u32>() - 1) != 0 {
            return Err(SystemError::EINVAL);
        }

        // 目前address指向所在页面的起始地址
        address -= offset;

        // 非共享：使用地址空间+页首虚拟地址作为私有键
        if !fshared {
            let address_space = AddressSpace::current()?;
            let key = FutexKey {
                ptr: 0,
                word: 0,
                offset: offset as u32,
                key: InnerFutexKey::Private(PrivateKey {
                    address: address as u64,
                    address_space: Some(Arc::downgrade(&address_space)),
                }),
            };
            return Ok(key);
        }

        // 共享：需要生成能跨进程匹配的键
        // 按照 Linux 语义，共享 futex 基于物理页帧号（PFN）或文件身份
        let address_space = AddressSpace::current()?;
        let as_guard = address_space
            .read_guard_no_reservation_conflict(VirtRegion::new(uaddr, MMArch::PAGE_SIZE));
        let vma = as_guard
            .mappings
            .contains(uaddr)
            .ok_or(SystemError::EINVAL)?;
        let vma_guard = vma.lock();

        // 页内索引（相对VMA起始地址）
        let page_index =
            ((uaddr.data() - vma_guard.region().start().data()) >> MMArch::PAGE_SHIFT) as u64;

        if let Some(file) = vma_guard.vm_file() {
            // 共享文件映射：使用 inode 唯一标识 + 文件页偏移
            let md = file.metadata()?;
            let dev = md.dev_id as u64;
            let ino = md.inode_id.into() as u64;
            let base_pgoff = vma_guard.backing_page_offset().unwrap_or(0) as u64;
            let shared = SharedKey {
                kind: SharedKeyKind::File { dev, ino },
                page_offset: base_pgoff + page_index,
            };
            let key = FutexKey {
                ptr: 0,
                word: 0,
                offset: offset as u32,
                key: InnerFutexKey::Shared(shared),
            };
            return Ok(key);
        } else {
            // 匿名映射（包括栈、堆、匿名mmap等）
            if let Some(shared_anon) = &vma_guard.shared_anon {
                // 显式共享的匿名映射（MAP_SHARED | MAP_ANONYMOUS）
                let shared = SharedKey {
                    kind: SharedKeyKind::SharedAnon { id: shared_anon.id },
                    page_offset: page_index,
                };
                let key = FutexKey {
                    ptr: 0,
                    word: 0,
                    offset: offset as u32,
                    key: InnerFutexKey::Shared(shared),
                };
                return Ok(key);
            } else {
                // 私有匿名映射（栈、堆等）+ FUTEX_SHARED 标志
                //
                // 按照 Linux 内核的实际实现（kernel/futex/core.c: get_futex_key）：
                // 对于匿名页的 FUTEX_SHARED，Linux 仍然使用 mm + 虚拟地址作为 key
                // （只是添加了一个 FUT_OFF_MMSHARED 标记）
                //
                // 这种设计的原因：
                // 1. 栈/堆这种私有匿名映射本质上不能跨进程共享
                // 2. 只能在同一进程的线程间同步（它们共享地址空间）
                // 3. 使用虚拟地址而非物理地址，与 swap 机制兼容
                //
                // DragonOS 的实现：
                // 使用 AddressSpace 的全局唯一 ID + 虚拟页号作为 shared key
                // - 同一进程的线程共享 AddressSpace，因此会生成相同的 key
                // - 不同进程的 AddressSpace 有不同的 ID，即使虚拟地址相同也不会冲突
                // - AddressSpace ID 是递增分配的，永不重复，避免了地址重用问题

                let address_space = AddressSpace::current()?;
                let as_id = address_space.id();

                drop(vma_guard);
                drop(as_guard);

                let shared = SharedKey {
                    kind: SharedKeyKind::PrivateAnonShared { as_id },
                    // 使用虚拟页号（不是物理页号！）
                    page_offset: (address >> MMArch::PAGE_SHIFT) as u64,
                };

                let key = FutexKey {
                    ptr: 0,
                    word: 0,
                    offset: offset as u32,
                    key: InnerFutexKey::Shared(shared),
                };
                return Ok(key);
            }
        }
    }

    pub fn futex_atomic_op_inuser(encoded_op: u32, uaddr: VirtAddr) -> Result<bool, SystemError> {
        let op = FutexOP::from_bits((encoded_op & 0x70000000) >> 28).ok_or(SystemError::ENOSYS)?;
        let cmp =
            FutexOpCMP::from_bits((encoded_op & 0x0f000000) >> 24).ok_or(SystemError::ENOSYS)?;

        let sign_extend32 = |value: u32, index: u32| -> i32 {
            let shift = 31 - index;
            return ((value << shift) as i32) >> shift;
        };

        let mut oparg = sign_extend32((encoded_op & 0x00fff000) >> 12, 11);
        let cmparg = sign_extend32(encoded_op & 0x00000fff, 11);

        if encoded_op & (FutexOP::FUTEX_OP_OPARG_SHIFT.bits() << 28) != 0 {
            if !(0..=31).contains(&oparg) {
                warn!(
                    "futex_wake_op: pid:{} tries to shift op by {}; fix this program",
                    ProcessManager::current_pcb().raw_pid().data(),
                    oparg
                );

                oparg &= 31;
            }
            oparg = 1i32 << oparg;
        }

        let old_val = Self::arch_futex_atomic_op_inuser(op, oparg as u32, uaddr)? as i32;

        match cmp {
            FutexOpCMP::FUTEX_OP_CMP_EQ => {
                return Ok(old_val == cmparg);
            }
            FutexOpCMP::FUTEX_OP_CMP_NE => {
                return Ok(old_val != cmparg);
            }
            FutexOpCMP::FUTEX_OP_CMP_LT => {
                return Ok(old_val < cmparg);
            }
            FutexOpCMP::FUTEX_OP_CMP_LE => {
                return Ok(old_val <= cmparg);
            }
            FutexOpCMP::FUTEX_OP_CMP_GE => {
                return Ok(old_val >= cmparg);
            }
            FutexOpCMP::FUTEX_OP_CMP_GT => {
                return Ok(old_val > cmparg);
            }
            _ => {
                return Err(SystemError::ENOSYS);
            }
        }
    }

    /// ### 对futex进行操作
    ///
    /// 进入该方法会关闭中断保证修改的原子性，所以进入该方法前应确保中断锁已释放
    ///
    /// ### return uaddr原来的值
    #[allow(unused_assignments)]
    pub fn arch_futex_atomic_op_inuser(
        op: FutexOP,
        oparg: u32,
        uaddr: VirtAddr,
    ) -> Result<u32, SystemError> {
        access_ok(uaddr, core::mem::size_of::<u32>()).map_err(|_| SystemError::EFAULT)?;

        let _pagefault_guard = PageFaultDisabledGuard::new();
        unsafe { Self::arch_futex_atomic_op_inuser_nofault(op, oparg, uaddr.as_ptr::<u32>()) }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn arch_futex_atomic_op_inuser_nofault(
        op: FutexOP,
        oparg: u32,
        uaddr: *mut u32,
    ) -> Result<u32, SystemError> {
        match op {
            FutexOP::FUTEX_OP_SET => Self::futex_atomic_xchg_nofault(uaddr, oparg),
            FutexOP::FUTEX_OP_ADD => Self::futex_atomic_xadd_nofault(uaddr, oparg),
            FutexOP::FUTEX_OP_OR => Self::futex_atomic_cmpxchg_or_nofault(uaddr, oparg),
            FutexOP::FUTEX_OP_ANDN => Self::futex_atomic_cmpxchg_and_nofault(uaddr, !oparg),
            FutexOP::FUTEX_OP_XOR => Self::futex_atomic_cmpxchg_xor_nofault(uaddr, oparg),
            _ => Err(SystemError::ENOSYS),
        }
    }

    #[cfg(target_arch = "riscv64")]
    unsafe fn arch_futex_atomic_op_inuser_nofault(
        op: FutexOP,
        oparg: u32,
        uaddr: *mut u32,
    ) -> Result<u32, SystemError> {
        match op {
            FutexOP::FUTEX_OP_SET => Self::riscv64_futex_amoswap_nofault(uaddr, oparg),
            FutexOP::FUTEX_OP_ADD => Self::riscv64_futex_amoadd_nofault(uaddr, oparg),
            FutexOP::FUTEX_OP_OR => Self::riscv64_futex_amoor_nofault(uaddr, oparg),
            FutexOP::FUTEX_OP_ANDN => Self::riscv64_futex_amoand_nofault(uaddr, !oparg),
            FutexOP::FUTEX_OP_XOR => Self::riscv64_futex_amoxor_nofault(uaddr, oparg),
            _ => Err(SystemError::ENOSYS),
        }
    }

    #[cfg(target_arch = "loongarch64")]
    unsafe fn arch_futex_atomic_op_inuser_nofault(
        op: FutexOP,
        oparg: u32,
        uaddr: *mut u32,
    ) -> Result<u32, SystemError> {
        match op {
            FutexOP::FUTEX_OP_SET => Self::loongarch64_futex_set_nofault(uaddr, oparg),
            FutexOP::FUTEX_OP_ADD => Self::loongarch64_futex_add_nofault(uaddr, oparg),
            FutexOP::FUTEX_OP_OR => Self::loongarch64_futex_or_nofault(uaddr, oparg),
            FutexOP::FUTEX_OP_ANDN => Self::loongarch64_futex_and_nofault(uaddr, !oparg),
            FutexOP::FUTEX_OP_XOR => Self::loongarch64_futex_xor_nofault(uaddr, oparg),
            _ => Err(SystemError::ENOSYS),
        }
    }

    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )))]
    unsafe fn arch_futex_atomic_op_inuser_nofault(
        _op: FutexOP,
        _oparg: u32,
        _uaddr: *mut u32,
    ) -> Result<u32, SystemError> {
        Err(SystemError::ENOSYS)
    }

    #[cfg(target_arch = "riscv64")]
    unsafe fn riscv64_futex_amoswap_nofault(
        uaddr: *mut u32,
        value: u32,
    ) -> Result<u32, SystemError> {
        let mut oldval: usize;
        let mut fault = 0usize;
        let value = value as usize;
        asm!(
            "2:",
            "amoswap.w.aqrl {oldval}, {value}, ({ptr})",
            "j 3f",
            "4:",
            "li {fault}, 1",
            "3:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            value = in(reg) value,
            oldval = out(reg) oldval,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(oldval as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "riscv64")]
    unsafe fn riscv64_futex_amoadd_nofault(
        uaddr: *mut u32,
        value: u32,
    ) -> Result<u32, SystemError> {
        let mut oldval: usize;
        let mut fault = 0usize;
        let value = value as usize;
        asm!(
            "2:",
            "amoadd.w.aqrl {oldval}, {value}, ({ptr})",
            "j 3f",
            "4:",
            "li {fault}, 1",
            "3:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            value = in(reg) value,
            oldval = out(reg) oldval,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(oldval as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "riscv64")]
    unsafe fn riscv64_futex_amoor_nofault(uaddr: *mut u32, value: u32) -> Result<u32, SystemError> {
        let mut oldval: usize;
        let mut fault = 0usize;
        let value = value as usize;
        asm!(
            "2:",
            "amoor.w.aqrl {oldval}, {value}, ({ptr})",
            "j 3f",
            "4:",
            "li {fault}, 1",
            "3:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            value = in(reg) value,
            oldval = out(reg) oldval,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(oldval as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "riscv64")]
    unsafe fn riscv64_futex_amoand_nofault(
        uaddr: *mut u32,
        value: u32,
    ) -> Result<u32, SystemError> {
        let mut oldval: usize;
        let mut fault = 0usize;
        let value = value as usize;
        asm!(
            "2:",
            "amoand.w.aqrl {oldval}, {value}, ({ptr})",
            "j 3f",
            "4:",
            "li {fault}, 1",
            "3:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            value = in(reg) value,
            oldval = out(reg) oldval,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(oldval as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "riscv64")]
    unsafe fn riscv64_futex_amoxor_nofault(
        uaddr: *mut u32,
        value: u32,
    ) -> Result<u32, SystemError> {
        let mut oldval: usize;
        let mut fault = 0usize;
        let value = value as usize;
        asm!(
            "2:",
            "amoxor.w.aqrl {oldval}, {value}, ({ptr})",
            "j 3f",
            "4:",
            "li {fault}, 1",
            "3:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            value = in(reg) value,
            oldval = out(reg) oldval,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(oldval as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "loongarch64")]
    unsafe fn loongarch64_futex_set_nofault(
        uaddr: *mut u32,
        value: u32,
    ) -> Result<u32, SystemError> {
        let mut oldval: usize;
        let mut fault = 0usize;
        let value = value as usize;
        asm!(
            "2:",
            "ll.w {oldval}, {ptr}, 0",
            "or {tmp}, {value}, $zero",
            "3:",
            "sc.w {tmp}, {ptr}, 0",
            "beqz {tmp}, 2b",
            "b 5f",
            "4:",
            "ori {fault}, $zero, 1",
            "5:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".dword 2b - .",
            ".dword 4b - . + 8",
            ".dword 3b - .",
            ".dword 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            value = in(reg) value,
            oldval = out(reg) oldval,
            tmp = out(reg) _,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(oldval as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "loongarch64")]
    unsafe fn loongarch64_futex_add_nofault(
        uaddr: *mut u32,
        value: u32,
    ) -> Result<u32, SystemError> {
        let mut oldval: usize;
        let mut fault = 0usize;
        let value = value as usize;
        asm!(
            "2:",
            "ll.w {oldval}, {ptr}, 0",
            "add.w {tmp}, {oldval}, {value}",
            "3:",
            "sc.w {tmp}, {ptr}, 0",
            "beqz {tmp}, 2b",
            "b 5f",
            "4:",
            "ori {fault}, $zero, 1",
            "5:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".dword 2b - .",
            ".dword 4b - . + 8",
            ".dword 3b - .",
            ".dword 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            value = in(reg) value,
            oldval = out(reg) oldval,
            tmp = out(reg) _,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(oldval as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "loongarch64")]
    unsafe fn loongarch64_futex_or_nofault(
        uaddr: *mut u32,
        value: u32,
    ) -> Result<u32, SystemError> {
        let mut oldval: usize;
        let mut fault = 0usize;
        let value = value as usize;
        asm!(
            "2:",
            "ll.w {oldval}, {ptr}, 0",
            "or {tmp}, {oldval}, {value}",
            "3:",
            "sc.w {tmp}, {ptr}, 0",
            "beqz {tmp}, 2b",
            "b 5f",
            "4:",
            "ori {fault}, $zero, 1",
            "5:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".dword 2b - .",
            ".dword 4b - . + 8",
            ".dword 3b - .",
            ".dword 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            value = in(reg) value,
            oldval = out(reg) oldval,
            tmp = out(reg) _,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(oldval as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "loongarch64")]
    unsafe fn loongarch64_futex_and_nofault(
        uaddr: *mut u32,
        value: u32,
    ) -> Result<u32, SystemError> {
        let mut oldval: usize;
        let mut fault = 0usize;
        let value = value as usize;
        asm!(
            "2:",
            "ll.w {oldval}, {ptr}, 0",
            "and {tmp}, {oldval}, {value}",
            "3:",
            "sc.w {tmp}, {ptr}, 0",
            "beqz {tmp}, 2b",
            "b 5f",
            "4:",
            "ori {fault}, $zero, 1",
            "5:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".dword 2b - .",
            ".dword 4b - . + 8",
            ".dword 3b - .",
            ".dword 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            value = in(reg) value,
            oldval = out(reg) oldval,
            tmp = out(reg) _,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(oldval as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "loongarch64")]
    unsafe fn loongarch64_futex_xor_nofault(
        uaddr: *mut u32,
        value: u32,
    ) -> Result<u32, SystemError> {
        let mut oldval: usize;
        let mut fault = 0usize;
        let value = value as usize;
        asm!(
            "2:",
            "ll.w {oldval}, {ptr}, 0",
            "xor {tmp}, {oldval}, {value}",
            "3:",
            "sc.w {tmp}, {ptr}, 0",
            "beqz {tmp}, 2b",
            "b 5f",
            "4:",
            "ori {fault}, $zero, 1",
            "5:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".dword 2b - .",
            ".dword 4b - . + 8",
            ".dword 3b - .",
            ".dword 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            value = in(reg) value,
            oldval = out(reg) oldval,
            tmp = out(reg) _,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(oldval as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn futex_atomic_xchg_nofault(
        uaddr: *mut u32,
        mut value: u32,
    ) -> Result<u32, SystemError> {
        let mut fault = 0usize;
        asm!(
            "2:",
            "xchg dword ptr [{ptr}], {value:e}",
            "jmp 3f",
            "4:",
            "mov {fault}, 1",
            "3:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            value = inout(reg) value,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(value)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn futex_atomic_xadd_nofault(
        uaddr: *mut u32,
        mut value: u32,
    ) -> Result<u32, SystemError> {
        let mut fault = 0usize;
        asm!(
            "2:",
            "lock xadd dword ptr [{ptr}], {value:e}",
            "jmp 3f",
            "4:",
            "mov {fault}, 1",
            "3:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            value = inout(reg) value,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(value)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn futex_atomic_cmpxchg_or_nofault(
        uaddr: *mut u32,
        oparg: u32,
    ) -> Result<u32, SystemError> {
        let mut oldval: u32;
        let mut fault = 0usize;
        asm!(
            "2:",
            "mov eax, dword ptr [{ptr}]",
            "3:",
            "mov {tmp:e}, eax",
            "or {tmp:e}, {oparg:e}",
            "5:",
            "lock cmpxchg dword ptr [{ptr}], {tmp:e}",
            "jnz 3b",
            "jmp 4f",
            "6:",
            "mov {fault}, 1",
            "4:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 6b - . + 8",
            ".quad 5b - .",
            ".quad 6b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            oparg = in(reg) oparg,
            tmp = out(reg) _,
            out("eax") oldval,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(oldval)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn futex_atomic_cmpxchg_and_nofault(
        uaddr: *mut u32,
        oparg: u32,
    ) -> Result<u32, SystemError> {
        let mut oldval: u32;
        let mut fault = 0usize;
        asm!(
            "2:",
            "mov eax, dword ptr [{ptr}]",
            "3:",
            "mov {tmp:e}, eax",
            "and {tmp:e}, {oparg:e}",
            "5:",
            "lock cmpxchg dword ptr [{ptr}], {tmp:e}",
            "jnz 3b",
            "jmp 4f",
            "6:",
            "mov {fault}, 1",
            "4:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 6b - . + 8",
            ".quad 5b - .",
            ".quad 6b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            oparg = in(reg) oparg,
            tmp = out(reg) _,
            out("eax") oldval,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(oldval)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn futex_atomic_cmpxchg_xor_nofault(
        uaddr: *mut u32,
        oparg: u32,
    ) -> Result<u32, SystemError> {
        let mut oldval: u32;
        let mut fault = 0usize;
        asm!(
            "2:",
            "mov eax, dword ptr [{ptr}]",
            "3:",
            "mov {tmp:e}, eax",
            "xor {tmp:e}, {oparg:e}",
            "5:",
            "lock cmpxchg dword ptr [{ptr}], {tmp:e}",
            "jnz 3b",
            "jmp 4f",
            "6:",
            "mov {fault}, 1",
            "4:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 6b - . + 8",
            ".quad 5b - .",
            ".quad 6b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            oparg = in(reg) oparg,
            tmp = out(reg) _,
            out("eax") oldval,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(oldval)
        } else {
            Err(SystemError::EFAULT)
        }
    }
}

//用于指示在处理robust list是最多处理多少个条目
const ROBUST_LIST_LIMIT: isize = 2048;

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct PosixRobustList {
    next: VirtAddr,
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub struct PosixRobustListHead {
    list: PosixRobustList,
    futex_offset: isize,
    list_op_pending: VirtAddr,
}

#[derive(Debug, Copy, Clone)]
pub struct RobustListHead {
    pub posix: PosixRobustListHead,
    pub uaddr: VirtAddr,
}

impl Deref for RobustListHead {
    type Target = PosixRobustListHead;

    fn deref(&self) -> &Self::Target {
        &self.posix
    }
}

impl DerefMut for RobustListHead {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.posix
    }
}

impl RobustListHead {
    /// # 获得futex的用户空间地址
    pub fn futex_uaddr(&self, entry: VirtAddr) -> VirtAddr {
        return VirtAddr::new(entry.data() + self.futex_offset as usize);
    }

    /// #获得list_op_peding的用户空间地址
    pub fn pending_uaddr(&self) -> Option<VirtAddr> {
        if self.list_op_pending.is_null() {
            return None;
        } else {
            return Some(self.futex_uaddr(self.list_op_pending));
        }
    }

    /// # 在内核注册robust list
    /// ## 参数
    /// - head_uaddr：robust list head用户空间地址
    /// - len：robust list head的长度    
    pub fn set_robust_list(head_uaddr: VirtAddr, len: usize) -> Result<usize, SystemError> {
        let robust_list_head_len = mem::size_of::<PosixRobustListHead>();
        if unlikely(len != robust_list_head_len) {
            return Err(SystemError::EINVAL);
        }

        let user_buffer_reader = UserBufferReader::new(
            head_uaddr.as_ptr::<PosixRobustListHead>(),
            mem::size_of::<PosixRobustListHead>(),
            true,
        )?;
        // 使用异常表保护读取，避免用户地址缺页/无效导致内核崩溃
        let robust_list_head = user_buffer_reader
            .buffer_protected(0)?
            .read_one::<PosixRobustListHead>(0)?;
        let robust_list_head = RobustListHead {
            posix: robust_list_head,
            uaddr: head_uaddr,
        };
        // 向内核注册robust list
        ProcessManager::current_pcb().set_robust_list(Some(robust_list_head));

        return Ok(0);
    }

    /// # 获取robust list head到用户空间
    /// ## 参数
    /// - pid：当前进程/线程的pid
    /// - head_ptr_uaddr: 指向用户空间指针的指针(即 struct robust_list_head **head_ptr)
    /// - len_ptr_uaddr: 指向用户空间size_t的指针(即 size_t *len_ptr)
    ///
    /// ## 返回
    /// - Ok(0) 成功
    /// - Err(SystemError) 失败
    ///
    /// ## 说明
    /// 该函数将目标进程的robust list head的用户空间地址写入到*head_ptr_uaddr,
    /// 将robust list head的大小写入到*len_ptr_uaddr
    ///
    /// ## 注意
    /// 根据Linux的行为，即使没有设置robust list，该函数也应该返回成功(0)，
    /// 并将head设置为NULL/0，len设置为sizeof(PosixRobustListHead)。
    /// 这是为了让用户态程序能够检测内核是否支持robust futex功能。
    pub fn get_robust_list(
        pid: usize,
        head_ptr_uaddr: VirtAddr,
        len_ptr_uaddr: VirtAddr,
    ) -> Result<usize, SystemError> {
        // 获取当前进程的process control block
        let pcb: Arc<ProcessControlBlock> = if pid == 0 {
            ProcessManager::current_pcb()
        } else {
            ProcessManager::find_task_by_vpid(RawPid::new(pid)).ok_or(SystemError::ESRCH)?
        };

        // TODO: 检查当前进程是否能ptrace另一个进程
        let ptrace = true;
        if !ptrace {
            return Err(SystemError::EPERM);
        }

        // 将len(即sizeof(PosixRobustListHead))拷贝到用户空间len_ptr
        let mut user_writer = UserBufferWriter::new(
            len_ptr_uaddr.as_ptr::<usize>(),
            core::mem::size_of::<usize>(),
            true,
        )?;
        // 使用异常表保护写回
        user_writer
            .buffer_protected(0)?
            .write_one::<usize>(0, &mem::size_of::<PosixRobustListHead>())?;

        // 获取当前线程的robust list head
        let robust_list_head_opt = *pcb.get_robust_list();

        // 将robust list head的用户空间地址拷贝到用户空间head_ptr
        // 注意: head_ptr_uaddr是二级指针,我们要写入的是robust_list_head.uaddr(一级指针)
        // 如果没有设置robust list，则写入0（NULL）
        let head_uaddr = robust_list_head_opt.map(|rh| rh.uaddr.data()).unwrap_or(0);

        let mut user_writer = UserBufferWriter::new(
            head_ptr_uaddr.as_ptr::<usize>(),
            mem::size_of::<usize>(),
            true,
        )?;
        // 使用异常表保护写回
        user_writer
            .buffer_protected(0)?
            .write_one::<usize>(0, &head_uaddr)?;

        return Ok(0);
    }

    /// # 进程/线程退出时清理工作
    /// ## 参数
    /// - current：当前进程/线程的pcb
    /// - pid：当前进程/线程的pid
    pub fn exit_robust_list(pcb: Arc<ProcessControlBlock>) {
        //指向当前进程的robust list头部的指针
        let head_info = match *pcb.get_robust_list() {
            Some(rl) => rl,
            None => {
                return;
            }
        };

        // 重新从用户空间读取 robust list head 的最新内容
        // 因为用户态可能在锁定 mutex 后已经更新了链表
        let user_buffer_reader = match UserBufferReader::new(
            head_info.uaddr.as_ptr::<PosixRobustListHead>(),
            core::mem::size_of::<PosixRobustListHead>(),
            true,
        ) {
            Ok(reader) => reader,
            Err(_) => {
                return;
            }
        };

        let posix_head = match user_buffer_reader
            .buffer_protected(0)
            .and_then(|b| b.read_one::<PosixRobustListHead>(0))
        {
            Ok(head) => head,
            Err(_) => {
                return;
            }
        };

        let head = RobustListHead {
            posix: posix_head,
            uaddr: head_info.uaddr,
        };

        // 遍历当前进程/线程的robust list
        for futex_uaddr in head.futexes() {
            let pid = pcb.raw_pid().into() as u32;
            match Self::handle_pi_futex_death(futex_uaddr, pid) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(_) => return,
            }
            let ret = Self::handle_futex_death(futex_uaddr, pid);
            if ret.is_err() {
                return;
            }
        }
        pcb.set_robust_list(None);
    }

    /// # 返回robust list的迭代器，将robust list list_op_pending 放到最后（如果存在）
    fn futexes(&self) -> FutexIterator<'_> {
        return FutexIterator::new(self);
    }

    /// # 安全地从用户空间读取任意类型的值，如果地址无效则返回None
    pub fn safe_read<T>(addr: VirtAddr) -> Option<UserBufferReader<'static>> {
        // 检查地址是否有效
        if addr.is_null() {
            return None;
        }

        let size = core::mem::size_of::<T>();
        return UserBufferReader::new_checked(addr.as_ptr::<T>(), size, true).ok();
    }

    /// # 安全地从用户空间读取u32值，如果地址无效则返回None
    fn safe_read_u32(addr: VirtAddr) -> Option<u32> {
        let reader =
            UserBufferReader::new(addr.as_ptr::<u32>(), core::mem::size_of::<u32>(), true).ok()?;
        reader.buffer_protected(0).ok()?.read_one::<u32>(0).ok()
    }

    /// # 处理进程即将死亡时，进程已经持有的futex，唤醒其他等待该futex的线程
    /// ## 参数
    /// - futex_uaddr：futex的用户空间地址
    /// - pid: 当前进程/线程的pid
    fn handle_futex_death(futex_uaddr: VirtAddr, pid: u32) -> Result<usize, SystemError> {
        // 安全地读取futex值
        let futex_val = match Self::safe_read_u32(futex_uaddr) {
            Some(val) => val,
            None => {
                // 地址无效，跳过此futex
                return Ok(0);
            }
        };

        let mut uval = futex_val;

        // 获取futex的原子操作指针
        // 使用 AtomicU32::from_ptr() 从原始指针创建原子操作对象
        // 注意：这里我们已经通过safe_read验证了地址的有效性
        let atomic_futex = unsafe { AtomicU32::from_ptr(futex_uaddr.as_ptr::<u32>()) };

        loop {
            // 该futex可能被其他进程占有
            let owner = uval & FUTEX_TID_MASK;
            if owner != pid {
                break;
            }

            // 计算新值: 保留FUTEX_WAITERS标志，设置FUTEX_OWNER_DIED，清除TID
            let mval = (uval & FUTEX_WAITERS) | FUTEX_OWNER_DIED;

            // 使用真正的原子CAS操作
            match atomic_futex.compare_exchange(uval, mval, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => {
                    // CAS成功，检查是否需要唤醒等待者
                    if mval & FUTEX_WAITERS != 0 {
                        let mut flags = FutexFlag::FLAGS_MATCH_NONE;
                        flags.insert(FutexFlag::FLAGS_SHARED);
                        // 唤醒操作可能会失败，但不影响流程
                        let _ = Futex::futex_wake(futex_uaddr, flags, 1, FUTEX_BITSET_MATCH_ANY);
                    }
                    break;
                }
                Err(current) => {
                    // CAS失败，说明值被其他线程修改了，更新uval并重试
                    uval = current;
                    continue;
                }
            }
        }

        return Ok(0);
    }

    /// # 处理 PI futex 的 owner death，唤醒 PI 等待者并设置 OWNER_DIED
    ///
    /// 返回值：
    /// - Ok(true): 已处理为 PI futex
    /// - Ok(false): 不是 PI futex（或不需要处理）
    fn handle_pi_futex_death(futex_uaddr: VirtAddr, pid: u32) -> Result<bool, SystemError> {
        let futex_val = match Self::safe_read_u32(futex_uaddr) {
            Some(val) => val,
            None => return Ok(false),
        };

        let mut uval = futex_val;
        let atomic_futex = unsafe { AtomicU32::from_ptr(futex_uaddr.as_ptr::<u32>()) };

        let key_private = Futex::get_futex_key(futex_uaddr, false, FutexAccess::FutexWrite).ok();
        let key_shared = Futex::get_futex_key(futex_uaddr, true, FutexAccess::FutexWrite).ok();

        let key = {
            let guard = FutexData::futex_map();
            let mut chosen = None;
            if let Some(k) = key_private.as_ref() {
                if guard.get(k).is_some() {
                    chosen = Some(k.clone());
                }
            }
            if chosen.is_none() {
                if let Some(k) = key_shared.as_ref() {
                    if guard.get(k).is_some() {
                        chosen = Some(k.clone());
                    }
                }
            }
            drop(guard);
            chosen
        };

        loop {
            let owner = uval & FUTEX_TID_MASK;
            if owner != pid {
                return Ok(false);
            }

            if let Some(ref key) = key {
                let mut futex_map_guard = FutexData::futex_map();
                let bucket = match futex_map_guard.get_mut(key) {
                    Some(bucket) => bucket,
                    None => {
                        drop(futex_map_guard);
                        let mval = FUTEX_OWNER_DIED;
                        match atomic_futex.compare_exchange(
                            uval,
                            mval,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        ) {
                            Ok(_) => return Ok(true),
                            Err(current) => {
                                uval = current;
                                continue;
                            }
                        }
                    }
                };

                if bucket.pi_owner == 0 && bucket.pi_waiters.is_empty() {
                    drop(futex_map_guard);
                    return Ok(false);
                }

                if bucket.pi_waiters.is_empty() {
                    let mval = FUTEX_OWNER_DIED;
                    match atomic_futex.compare_exchange(
                        uval,
                        mval,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => {
                            bucket.pi_owner = 0;
                            drop(futex_map_guard);
                            FutexData::try_remove(key);
                            return Ok(true);
                        }
                        Err(current) => {
                            uval = current;
                            continue;
                        }
                    }
                }

                let next_waiter = bucket.pi_waiters.pop_front();
                if let Some(next_waiter) = next_waiter {
                    let has_more = !bucket.pi_waiters.is_empty();
                    let new_val = next_waiter.tid
                        | FUTEX_OWNER_DIED
                        | if has_more { FUTEX_WAITERS } else { 0 };
                    match atomic_futex.compare_exchange(
                        uval,
                        new_val,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => {
                            bucket.pi_owner = next_waiter.tid;
                            drop(futex_map_guard);
                            next_waiter.waker.wake();
                            return Ok(true);
                        }
                        Err(current) => {
                            bucket.pi_waiters.push_front(next_waiter);
                            drop(futex_map_guard);
                            uval = current;
                            continue;
                        }
                    }
                }

                drop(futex_map_guard);
                continue;
            }

            let mval = FUTEX_OWNER_DIED;
            match atomic_futex.compare_exchange(uval, mval, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Ok(true),
                Err(current) => {
                    uval = current;
                    continue;
                }
            }
        }
    }
}

pub struct FutexIterator<'a> {
    robust_list_head: &'a RobustListHead,
    entry: VirtAddr,
    count: isize,
}

impl<'a> FutexIterator<'a> {
    pub fn new(robust_list_head: &'a RobustListHead) -> Self {
        return Self {
            robust_list_head,
            entry: robust_list_head.list.next,
            count: 0,
        };
    }

    fn is_end(&mut self) -> bool {
        return self.count < 0;
    }

    /// 检查是否到达链表末尾（entry 指回 head.list）
    fn is_sentinel(&self) -> bool {
        // 链表的哨兵是 &head.list，其地址就是 head.uaddr
        // 因为 list 是 head 结构的第一个字段
        self.entry.data() == self.robust_list_head.uaddr.data()
    }
}

impl Iterator for FutexIterator<'_> {
    type Item = VirtAddr;

    fn next(&mut self) -> Option<Self::Item> {
        if self.is_end() {
            return None;
        }

        // 如果初始 entry 就是哨兵，说明链表为空
        if self.count == 0 && self.is_sentinel() {
            self.count = -1;
            return self.robust_list_head.pending_uaddr();
        }

        while !self.is_sentinel() {
            if self.count >= ROBUST_LIST_LIMIT {
                break;
            }
            if self.entry.is_null() {
                return None;
            }

            //获取futex val地址
            let futex_uaddr = if self.entry.data() != self.robust_list_head.list_op_pending.data() {
                Some(self.robust_list_head.futex_uaddr(self.entry))
            } else {
                None
            };

            // 安全地读取下一个entry
            let next_entry = RobustListHead::safe_read::<PosixRobustList>(self.entry)
                .and_then(|reader| reader.read_one_from_user::<PosixRobustList>(0).ok())?;

            self.entry = next_entry.next;
            self.count += 1;

            if futex_uaddr.is_some() {
                return futex_uaddr;
            }
        }

        self.count = -1;
        self.robust_list_head.pending_uaddr()
    }
}
