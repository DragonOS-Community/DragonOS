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
    intrinsics::unlikely,
    mem,
    ops::{Deref, DerefMut},
};
use log::warn;

use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    arch::MMArch,
    libs::{
        resource_wait::{ResourceMutex, ResourceMutexGuard},
        spinlock::SpinLock,
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

#[path = "queue.rs"]
mod queue;
use queue::{Node as WaitNode, Queue as WaitQueueNodes};

#[cfg(feature = "mm_selftest")]
#[path = "wake_selftest.rs"]
mod selftest;

#[cfg(feature = "mm_selftest")]
#[path = "registration_selftest.rs"]
mod registration_selftest;

static mut FUTEX_DATA: Option<FutexData> = None;

pub struct FutexData {
    data: ResourceMutex<HashMap<FutexKey, FutexHashBucket>>,
}

impl FutexData {
    pub fn futex_map() -> ResourceMutexGuard<'static, HashMap<FutexKey, FutexHashBucket>> {
        unsafe { ResourceMutexGuard::lock(&FUTEX_DATA.as_ref().unwrap().data) }
    }

    fn remove_empty_locked(
        map: &mut HashMap<FutexKey, FutexHashBucket>,
        key: &FutexKey,
    ) -> Option<FutexHashBucket> {
        if map.get(key).is_some_and(FutexHashBucket::is_empty) {
            map.remove(key)
        } else {
            None
        }
    }

    pub fn try_remove(key: &FutexKey) -> Option<FutexHashBucket> {
        unsafe {
            let mut guard = ResourceMutexGuard::lock(&FUTEX_DATA.as_ref().unwrap().data);
            return Self::remove_empty_locked(&mut guard, key);
        }
    }
}

pub struct Futex;

// 对于同一个futex的进程或线程将会在这个bucket等待
pub struct FutexHashBucket {
    // 该futex维护的等待队列
    pub(super) chain: WaitQueueNodes<Arc<FutexObj>>,
    // PI futex state
    pub(super) pi_owner: u32,
    pub(super) pi_waiters: LinkedList<Arc<FutexObj>>,
}

impl FutexHashBucket {
    pub fn new() -> Self {
        Self {
            chain: WaitQueueNodes::new(),
            pi_owner: 0,
            pi_waiters: LinkedList::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.chain.is_empty() && self.pi_waiters.is_empty() && self.pi_owner == 0
    }

    #[cfg(feature = "mm_selftest")]
    #[inline(always)]
    pub fn enqueue(&mut self, futex_q: Arc<FutexObj>) {
        self.chain
            .push_node(WaitNode::try_new(futex_q).expect("futex test node allocation failed"));
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
        // Keep only the visited prefix separately. Stop at the wake budget,
        // then splice the untouched suffix back in O(1), preserving FIFO and
        // avoiding a full-bucket scan when the first waiter can be woken.
        let mut retained = WaitQueueNodes::new();
        while count < nr_wake {
            let Some(node) = self.chain.pop_node() else {
                break;
            };
            let waiter = node.value();
            let matches =
                *waiter.key.lock() == key && bitset.is_none_or(|mask| waiter.bitset & mask != 0);
            if matches {
                if waiter.waker.wake() {
                    count += 1;
                }
            } else {
                retained.push_node(node);
            }
        }
        retained.append(&mut self.chain);
        self.chain = retained;

        Ok(count as usize)
    }

    /// 将FutexObj从bucket中删除
    pub fn remove_by_waker(&mut self, waker: &Arc<Waker>) -> bool {
        let before = self.chain.len();
        for _ in 0..before {
            let node = self.chain.pop_node().unwrap();
            if !Arc::ptr_eq(&node.value().waker, waker) {
                self.chain.push_node(node);
            }
        }
        before != self.chain.len()
    }
}

#[derive(Debug)]
pub struct FutexObj {
    pub(super) waker: Arc<Waker>,
    // The map lock precedes this lock. Requeue updates the cancellation location.
    pub(super) key: SpinLock<FutexKey>,
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
                data: ResourceMutex::new(HashMap::new()),
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
        let mut charge_recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();

        loop {
            let creator = crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());
            let data_account = current_address_space.memory_account();
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
                let message = PageFaultMessage::new(
                    vma,
                    uaddr,
                    flags,
                    mapper,
                    current_address_space.clone(),
                    creator.clone(),
                    data_account.clone(),
                );
                PageFaultHandler::handle_mm_fault(message)
            };

            if let Some(failure) = fault.charge_failure {
                drop(space_guard);
                crate::mm::fault::recover_fault_charge(
                    failure,
                    &mut charge_recovery,
                    &data_account,
                    flags,
                )?;
                continue;
            }

            if fault.reason.contains(VmFaultReason::VM_FAULT_COMPLETED) {
                return Ok(());
            }
            if fault.reason.contains(VmFaultReason::VM_FAULT_RETRY) {
                flags |= FaultFlags::FAULT_FLAG_TRIED;
                let wait = fault.retry_wait;
                drop(space_guard);
                if let Some(wait) = wait {
                    let result = match wait.wait() {
                        Err(crate::mm::allocation::MemoryAllocationError::Charge(failure)) => {
                            crate::mm::fault::recover_fault_charge(
                                failure,
                                &mut charge_recovery,
                                &data_account,
                                flags,
                            )?;
                            continue;
                        }
                        Err(crate::mm::allocation::MemoryAllocationError::System(error)) => {
                            Err(error)
                        }
                        Ok(()) => Ok(()),
                    };
                    if let Err(error) = result {
                        if !matches!(error, SystemError::EINTR | SystemError::ERESTARTSYS) {
                            crate::mm::fault::account_fault_result(
                                &data_account,
                                flags,
                                VmFaultReason::VM_FAULT_SIGBUS,
                            );
                        }
                        return Err(SystemError::EFAULT);
                    }
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

        // Capture the expiry once: refault retries consume the original budget.
        // Keep validation/error precedence below the locked value comparison.
        let timer_deadline = abs_time
            .filter(PosixTimeSpec::is_valid_timeout)
            .map(|time| next_n_us_timer_jiffies(time.to_ktime_ns().div_ceil(1000)));
        let (key, futex_map_guard, uval) = loop {
            let key = Self::get_futex_key(
                uaddr,
                flags.contains(FutexFlag::FLAGS_SHARED),
                FutexAccess::FutexRead,
            )?;
            let map = FutexData::futex_map();
            match Self::read_user_u32_nofault(uaddr) {
                Ok(value) => break (key, map, value),
                Err(SystemError::EFAULT) => {
                    drop(map);
                    // Faulting and charge recovery must never hold the map lock.
                    // This value only faults the page in; retry both identity and
                    // the final comparison after reacquiring the map lock.
                    let reader = UserBufferReader::new(
                        uaddr.as_ptr::<u32>(),
                        core::mem::size_of::<u32>(),
                        true,
                    )?;
                    let _ = reader.read_one_from_user::<u32>(0)?;
                }
                Err(error) => return Err(error),
            }
        };

        // 不满足wait条件，返回错误
        if uval != val {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        // Validate before creating either a waiter or a bucket.
        if let Some(time) = abs_time {
            if !time.is_valid_timeout() {
                return Err(SystemError::EINVAL);
            }
            if timer_deadline.is_some_and(|deadline| deadline <= crate::time::timer::clock()) {
                return Err(SystemError::ETIMEDOUT);
            }
        }
        // Preparation may allocate, so release the resource lock and keep all
        // unpublished owners outside the final registration guard's lifetime.
        drop(futex_map_guard);
        let (waiter, waker) = Waiter::try_new_pair()?;
        let prepared_timer = if let Some(jiffies) = timer_deadline {
            let callback = Box::try_new(WakerTimer {
                waker: waker.clone(),
            })
            .map_err(|_| SystemError::ENOMEM)?;
            Some(Timer::try_prepare(callback, jiffies)?)
        } else {
            None
        };
        let timer = prepared_timer.as_ref().map(|prepared| prepared.timer());
        let futex_q = Arc::try_new(FutexObj {
            waker: waker.clone(),
            key: SpinLock::new(key),
            bitset,
            tid: 0,
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let node = WaitNode::try_new(futex_q.clone())?;

        // Both the shared mapping identity and the value may have changed while
        // preparing. Refault without the map guard, then resolve both anew.
        let (key, mut futex_map_guard) = loop {
            let key = Self::get_futex_key(
                uaddr,
                flags.contains(FutexFlag::FLAGS_SHARED),
                FutexAccess::FutexRead,
            )?;
            let map = FutexData::futex_map();
            match Self::read_user_u32_nofault(uaddr) {
                Ok(value) if value != val => return Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
                Ok(_) => break (key, map),
                Err(SystemError::EFAULT) => {
                    drop(map);
                    let reader = UserBufferReader::new(
                        uaddr.as_ptr::<u32>(),
                        core::mem::size_of::<u32>(),
                        true,
                    )?;
                    let _ = reader.read_one_from_user::<u32>(0)?;
                }
                Err(error) => return Err(error),
            }
        };
        if timer_deadline.is_some_and(|deadline| deadline <= crate::time::timer::clock()) {
            return Err(SystemError::ETIMEDOUT);
        }
        if !futex_map_guard.contains_key(&key) {
            futex_map_guard
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
        }
        *futex_q.key.lock() = key.clone();
        futex_map_guard
            .entry(key)
            .or_insert_with(FutexHashBucket::new)
            .chain
            .push_node(node);
        // Queue publication and timer activation are now allocation-free.
        if let Some(prepared) = prepared_timer {
            prepared.activate();
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
        let current_key = futex_q.key.lock().clone();
        let in_queue = futex_map_guard
            .get_mut(&current_key)
            .map(|bucket| bucket.remove_by_waker(&waker))
            .unwrap_or(false);
        let retired = FutexData::remove_empty_locked(&mut futex_map_guard, &current_key);

        drop(futex_map_guard);
        drop(retired);

        // 取消定时器
        if let Some(timer) = timer {
            timer.cancel();
        }

        // Linux gives successful unqueue by WAKE priority over a racing timer
        // or signal. Only a waiter still queued can report either interruption.
        if !in_queue {
            return Ok(0);
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
        let Some(count) = Self::wake_existing(&mut binding, &key, nr_wake, bitset)? else {
            return Ok(0);
        };

        let retired = FutexData::remove_empty_locked(&mut binding, &key);
        drop(binding);
        drop(retired);

        Ok(count)
    }

    /// Wake only an already registered key. Some(0) also covers an existing
    /// empty/PI-only bucket: the caller must still run ordinary empty cleanup.
    fn wake_existing(
        map: &mut HashMap<FutexKey, FutexHashBucket>,
        key: &FutexKey,
        nr_wake: u32,
        bitset: u32,
    ) -> Result<Option<usize>, SystemError> {
        let Some(bucket) = map.get_mut(key) else {
            return Ok(None);
        };
        // Preserve Linux FUTEX_WAKE's nr_wake == 0 behavior. Other bucket
        // users (for example requeue) retain their literal zero-wake budget.
        let effective_nr_wake = nr_wake.max(1);
        bucket
            .wake_up(key.clone(), Some(bitset), effective_nr_wake)
            .map(Some)
    }

    #[cfg(feature = "mm_selftest")]
    pub(crate) fn heap_denied_wake_selftest() -> Result<bool, SystemError> {
        selftest::run()
    }

    #[cfg(feature = "mm_selftest")]
    pub(crate) fn failed_registration_selftest() -> Result<bool, SystemError> {
        registration_selftest::run()
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

        let (key1, key2, mut map) = loop {
            let key1 = Self::get_futex_key(
                uaddr1,
                flags.contains(FutexFlag::FLAGS_SHARED),
                FutexAccess::FutexRead,
            )?;
            let key2 = Self::get_futex_key(
                uaddr2,
                flags.contains(FutexFlag::FLAGS_SHARED),
                FutexAccess::FutexRead,
            )?;
            let map = FutexData::futex_map();
            if let Some(expected) = cmpval {
                match Self::read_user_u32_nofault(uaddr1) {
                    Ok(value) if value != expected => {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                    Ok(_) => {}
                    Err(SystemError::EFAULT) => {
                        drop(map);
                        let reader = UserBufferReader::new(
                            uaddr1.as_ptr::<u32>(),
                            core::mem::size_of::<u32>(),
                            true,
                        )?;
                        let _ = reader.read_one_from_user::<u32>(0)?;
                        // Both shared mappings may have changed during fault-in.
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            break (key1, key2, map);
        };
        let (count, retired) = Self::requeue_existing(
            &mut map,
            &key1,
            &key2,
            nr_wake as usize,
            nr_requeue as usize,
        )?;
        drop(map);
        drop(retired);
        Ok(count)
    }

    /// Transfer existing queue nodes under the same map lock as registration.
    /// The caller retires the returned empty source after releasing that lock.
    fn requeue_existing(
        map: &mut HashMap<FutexKey, FutexHashBucket>,
        key1: &FutexKey,
        key2: &FutexKey,
        nr_wake: usize,
        nr_requeue: usize,
    ) -> Result<(usize, Option<FutexHashBucket>), SystemError> {
        let Some(source) = map.get(key1) else {
            return Ok((0, None));
        };
        // Reserve before waking anyone: ENOMEM must leave all waiters intact.
        let may_move = nr_requeue > 0 && source.chain.len() > nr_wake;
        if key1 != key2 && may_move && !map.contains_key(key2) {
            map.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        }
        let source = map.get_mut(key1).unwrap();
        let woken = source.wake_up(key1.clone(), None, nr_wake as u32)?;
        let count = source.chain.len().min(nr_requeue);
        if key1 != key2 && count > 0 {
            let rest = source.chain.split_off(count);
            let mut moved = mem::replace(&mut source.chain, rest);
            for waiter in &moved {
                *waiter.key.lock() = key2.clone();
            }
            map.entry(key2.clone())
                .or_insert_with(FutexHashBucket::new)
                .chain
                .append(&mut moved);
        }
        let retired = FutexData::remove_empty_locked(map, key1);
        Ok((woken + count, retired))
    }

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
                    wake_count += bucket2.wake_up(key2.clone(), None, nr_wake2)?;
                }
            }

            let retired1 = FutexData::remove_empty_locked(&mut futex_data_guard, &key1);
            let retired2 = FutexData::remove_empty_locked(&mut futex_data_guard, &key2);
            drop(futex_data_guard);
            drop((retired1, retired2));
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

        let file = vma_guard.vm_file();
        let anonymous_pages = file
            .as_ref()
            .is_some_and(|file| file.inode().mmap_uses_anonymous_pages());
        if anonymous_pages && !vma_guard.vm_flags().contains(VmFlags::VM_WRITE) {
            // Read-only anonymous pages cannot participate in shared futexes.
            // In particular, a retained /dev/zero file is not their identity.
            return Err(SystemError::EFAULT);
        }
        if let Some(file) = file.filter(|_| !anonymous_pages) {
            // 共享文件映射：使用 inode 唯一标识 + 文件页偏移
            let (dev, ino) = file.mapped_file_identity();
            let dev = dev as u64;
            let ino = ino.into() as u64;
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

    /// One aligned value load, with no prefault or sleeping fault under map.
    fn read_user_u32_nofault(uaddr: VirtAddr) -> Result<u32, SystemError> {
        if uaddr.data() & (core::mem::align_of::<u32>() - 1) != 0 {
            return Err(SystemError::EINVAL);
        }
        access_ok(uaddr, core::mem::size_of::<u32>()).map_err(|_| SystemError::EFAULT)?;
        let _pagefault_guard = PageFaultDisabledGuard::new();
        unsafe { Self::arch_read_user_u32_nofault(uaddr.as_ptr::<u32>()) }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn arch_read_user_u32_nofault(uaddr: *const u32) -> Result<u32, SystemError> {
        let mut value = 0usize;
        let mut fault = 0usize;
        asm!(
            "2:",
            "mov {value:e}, dword ptr [{ptr}]",
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
            Ok(value as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "riscv64")]
    unsafe fn arch_read_user_u32_nofault(uaddr: *const u32) -> Result<u32, SystemError> {
        let mut value = 0usize;
        let mut fault = 0usize;
        asm!(
            "2:",
            "lw {value}, 0({ptr})",
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
            value = inout(reg) value,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(value as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "loongarch64")]
    unsafe fn arch_read_user_u32_nofault(uaddr: *const u32) -> Result<u32, SystemError> {
        let mut value = 0usize;
        let mut fault = 0usize;
        asm!(
            "2:",
            "ld.wu {value}, {ptr}, 0",
            "b 3f",
            "4:",
            "ori {fault}, $zero, 1",
            "3:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".dword 2b - .",
            ".dword 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            value = inout(reg) value,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(value as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )))]
    unsafe fn arch_read_user_u32_nofault(_uaddr: *const u32) -> Result<u32, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn futex_cmpxchg_user(uaddr: VirtAddr, expected: u32, new: u32) -> Result<u32, SystemError> {
        if uaddr.data() & (core::mem::align_of::<u32>() - 1) != 0 {
            return Err(SystemError::EINVAL);
        }
        access_ok(uaddr, core::mem::size_of::<u32>()).map_err(|_| SystemError::EFAULT)?;

        let _pagefault_guard = PageFaultDisabledGuard::new();
        unsafe { Self::arch_futex_cmpxchg_nofault(uaddr.as_ptr::<u32>(), expected, new) }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn arch_futex_cmpxchg_nofault(
        uaddr: *mut u32,
        mut expected: u32,
        new: u32,
    ) -> Result<u32, SystemError> {
        let mut fault = 0usize;
        asm!(
            "2:",
            "lock cmpxchg dword ptr [{ptr}], {new:e}",
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
            new = in(reg) new,
            inout("eax") expected,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(expected)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "riscv64")]
    unsafe fn arch_futex_cmpxchg_nofault(
        uaddr: *mut u32,
        expected: u32,
        new: u32,
    ) -> Result<u32, SystemError> {
        let mut observed: usize;
        let mut fault = 0usize;
        asm!(
            "2:",
            "lr.w.aq {observed}, ({ptr})",
            "bne {observed}, {expected}, 5f",
            "3:",
            "sc.w.rl {status}, {new}, ({ptr})",
            "bnez {status}, 2b",
            "j 5f",
            "4:",
            "li {fault}, 1",
            "5:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 4b - . + 8",
            ".quad 3b - .",
            ".quad 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            expected = in(reg) expected as i32 as isize as usize,
            new = in(reg) new as usize,
            observed = out(reg) observed,
            status = out(reg) _,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(observed as u32)
        } else {
            Err(SystemError::EFAULT)
        }
    }

    #[cfg(target_arch = "loongarch64")]
    unsafe fn arch_futex_cmpxchg_nofault(
        uaddr: *mut u32,
        expected: u32,
        new: u32,
    ) -> Result<u32, SystemError> {
        let mut observed: usize;
        let mut fault = 0usize;
        asm!(
            "2:",
            "ll.w {observed}, {ptr}, 0",
            "bne {observed}, {expected}, 5f",
            "or {status}, {new}, $zero",
            "3:",
            "sc.w {status}, {ptr}, 0",
            "beqz {status}, 2b",
            "b 5f",
            "4:",
            "ori {fault}, $zero, 1",
            "5:",
            "dbar 0x700",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".dword 2b - .",
            ".dword 4b - . + 8",
            ".dword 3b - .",
            ".dword 4b - . + 8",
            ".popsection",
            ptr = in(reg) uaddr,
            expected = in(reg) expected as i32 as isize as usize,
            new = in(reg) new as usize,
            observed = out(reg) observed,
            status = out(reg) _,
            fault = inout(reg) fault,
            options(nostack)
        );
        if fault == 0 {
            Ok(observed as u32)
        } else {
            Err(SystemError::EFAULT)
        }
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

    pub fn cleanup_robust_list(pcb: &Arc<ProcessControlBlock>) {
        let Some(head) = pcb.take_robust_list() else {
            return;
        };
        Self::cleanup_robust_list_head(pcb, head);
    }

    pub(crate) fn cleanup_robust_list_head(
        pcb: &Arc<ProcessControlBlock>,
        head_info: RobustListHead,
    ) {
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

        let pid = pcb.task_pid_vnr().data() as u32;
        for entry in head.futexes() {
            let result = if entry.pi {
                Self::handle_pi_futex_death(entry.uaddr, pid).map(|_| ())
            } else {
                Self::handle_futex_death(entry.uaddr, pid, entry.pending).map(|_| ())
            };
            if result.is_err() {
                return;
            }
        }
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
    fn handle_futex_death(
        futex_uaddr: VirtAddr,
        pid: u32,
        pending: bool,
    ) -> Result<usize, SystemError> {
        // 安全地读取futex值
        let futex_val = match Self::safe_read_u32(futex_uaddr) {
            Some(val) => val,
            None => return Err(SystemError::EFAULT),
        };

        let mut uval = futex_val;

        loop {
            // 该futex可能被其他进程占有
            let owner = uval & FUTEX_TID_MASK;
            if pending && owner == 0 {
                let _ = Futex::futex_wake(
                    futex_uaddr,
                    FutexFlag::FLAGS_SHARED,
                    1,
                    FUTEX_BITSET_MATCH_ANY,
                );
                break;
            }
            if owner != pid {
                break;
            }

            // 计算新值: 保留FUTEX_WAITERS标志，设置FUTEX_OWNER_DIED，清除TID
            let mval = (uval & FUTEX_WAITERS) | FUTEX_OWNER_DIED;

            // 使用真正的原子CAS操作
            match Futex::futex_cmpxchg_user(futex_uaddr, uval, mval) {
                Ok(observed) if observed == uval => {
                    // CAS成功，检查是否需要唤醒等待者
                    if mval & FUTEX_WAITERS != 0 {
                        let mut flags = FutexFlag::FLAGS_MATCH_NONE;
                        flags.insert(FutexFlag::FLAGS_SHARED);
                        // 唤醒操作可能会失败，但不影响流程
                        let _ = Futex::futex_wake(futex_uaddr, flags, 1, FUTEX_BITSET_MATCH_ANY);
                    }
                    break;
                }
                Ok(observed) => {
                    uval = observed;
                    continue;
                }
                Err(err) => return Err(err),
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
            None => return Err(SystemError::EFAULT),
        };

        let mut uval = futex_val;
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
                        match Futex::futex_cmpxchg_user(futex_uaddr, uval, mval) {
                            Ok(observed) if observed == uval => return Ok(true),
                            Ok(observed) => {
                                uval = observed;
                                continue;
                            }
                            Err(err) => return Err(err),
                        }
                    }
                };

                if bucket.pi_owner == 0 && bucket.pi_waiters.is_empty() {
                    drop(futex_map_guard);
                    return Ok(false);
                }

                if bucket.pi_waiters.is_empty() {
                    let mval = FUTEX_OWNER_DIED;
                    match Futex::futex_cmpxchg_user(futex_uaddr, uval, mval) {
                        Ok(observed) if observed == uval => {
                            bucket.pi_owner = 0;
                            drop(futex_map_guard);
                            FutexData::try_remove(key);
                            return Ok(true);
                        }
                        Ok(observed) => {
                            uval = observed;
                            continue;
                        }
                        Err(err) => return Err(err),
                    }
                }

                let next_waiter = bucket.pi_waiters.front().cloned();
                if let Some(next_waiter) = next_waiter {
                    // Keep the list node until the user-word transition is
                    // committed; a CAS retry must not allocate a replacement.
                    let has_more = bucket.pi_waiters.len() > 1;
                    let new_val = next_waiter.tid
                        | FUTEX_OWNER_DIED
                        | if has_more { FUTEX_WAITERS } else { 0 };
                    match Futex::futex_cmpxchg_user(futex_uaddr, uval, new_val) {
                        Ok(observed) if observed == uval => {
                            let removed =
                                bucket.pi_waiters.pop_front().expect("observed PI waiter");
                            assert!(Arc::ptr_eq(&removed, &next_waiter));
                            bucket.pi_owner = next_waiter.tid;
                            drop(futex_map_guard);
                            next_waiter.waker.wake();
                            return Ok(true);
                        }
                        Ok(observed) => {
                            drop(futex_map_guard);
                            uval = observed;
                            continue;
                        }
                        Err(err) => return Err(err),
                    }
                }

                drop(futex_map_guard);
                continue;
            }

            let mval = FUTEX_OWNER_DIED;
            match Futex::futex_cmpxchg_user(futex_uaddr, uval, mval) {
                Ok(observed) if observed == uval => return Ok(true),
                Ok(observed) => {
                    uval = observed;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

struct FutexIterator<'a> {
    robust_list_head: &'a RobustListHead,
    entry: VirtAddr,
    count: isize,
    stop_after_current: bool,
}

struct RobustFutexEntry {
    uaddr: VirtAddr,
    pi: bool,
    pending: bool,
}

impl<'a> FutexIterator<'a> {
    pub fn new(robust_list_head: &'a RobustListHead) -> Self {
        return Self {
            robust_list_head,
            entry: robust_list_head.list.next,
            count: 0,
            stop_after_current: false,
        };
    }

    fn decode_entry(
        &self,
        encoded: VirtAddr,
        pending: bool,
    ) -> Result<RobustFutexEntry, SystemError> {
        if encoded.is_null() {
            return Err(SystemError::EFAULT);
        }
        let pi = encoded.data() & 1 != 0;
        let base = encoded.data() & !1;
        let address = base
            .checked_add_signed(self.robust_list_head.futex_offset)
            .ok_or(SystemError::EFAULT)?;
        if address & (core::mem::align_of::<u32>() - 1) != 0 {
            return Err(SystemError::EINVAL);
        }
        let uaddr = VirtAddr::new(address);
        access_ok(uaddr, core::mem::size_of::<u32>()).map_err(|_| SystemError::EFAULT)?;
        Ok(RobustFutexEntry { uaddr, pi, pending })
    }

    fn pending_entry(&self) -> Option<RobustFutexEntry> {
        if self.robust_list_head.list_op_pending.is_null() {
            return None;
        }
        self.decode_entry(self.robust_list_head.list_op_pending, true)
            .ok()
    }

    fn is_end(&mut self) -> bool {
        return self.count < 0;
    }

    /// 检查是否到达链表末尾（entry 指回 head.list）
    fn is_sentinel(&self) -> bool {
        // 链表的哨兵是 &head.list，其地址就是 head.uaddr
        // 因为 list 是 head 结构的第一个字段
        (self.entry.data() & !1) == self.robust_list_head.uaddr.data()
    }
}

impl Iterator for FutexIterator<'_> {
    type Item = RobustFutexEntry;

    fn next(&mut self) -> Option<Self::Item> {
        if self.is_end() || self.stop_after_current {
            self.count = -1;
            return None;
        }

        // 如果初始 entry 就是哨兵，说明链表为空
        if self.count == 0 && self.is_sentinel() {
            self.count = -1;
            return self.pending_entry();
        }

        while !self.is_sentinel() {
            if self.count >= ROBUST_LIST_LIMIT {
                break;
            }
            let encoded_entry = self.entry;
            if encoded_entry.is_null() {
                return None;
            }

            let entry_address = VirtAddr::new(encoded_entry.data() & !1);
            let pending_address = self.robust_list_head.list_op_pending.data() & !1;

            //获取futex val地址
            let futex_entry = if entry_address.data() != pending_address {
                match self.decode_entry(encoded_entry, false) {
                    Ok(entry) => Some(entry),
                    Err(_) => {
                        self.count = -1;
                        return None;
                    }
                }
            } else {
                None
            };

            // 安全地读取下一个entry
            let next_entry = RobustListHead::safe_read::<PosixRobustList>(entry_address)
                .and_then(|reader| reader.read_one_from_user::<PosixRobustList>(0).ok());

            let Some(next_entry) = next_entry else {
                self.stop_after_current = true;
                return futex_entry;
            };

            self.entry = next_entry.next;
            self.count += 1;

            if futex_entry.is_some() {
                return futex_entry;
            }
        }

        self.count = -1;
        self.pending_entry()
    }
}
