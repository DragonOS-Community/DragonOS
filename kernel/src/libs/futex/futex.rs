use alloc::{
    collections::LinkedList,
    sync::{Arc, Weak},
};
use core::hash::{Hash, Hasher};
use core::{
    intrinsics::{likely, unlikely},
    mem,
    sync::atomic::AtomicU64,
};
use log::warn;

use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    arch::{CurrentIrqArch, MMArch},
    exception::InterruptArch,
    libs::spinlock::{SpinLock, SpinLockGuard},
    mm::{ucontext::AddressSpace, MemoryManagementArch, VirtAddr},
    process::{Pid, ProcessControlBlock, ProcessManager},
    sched::{schedule, SchedMode},
    syscall::user_access::{UserBufferReader, UserBufferWriter},
    time::{
        timer::{next_n_us_timer_jiffies, Timer, WakeUpHelper},
        PosixTimeSpec,
    },
};

use super::constant::*;

static mut FUTEX_DATA: Option<FutexData> = None;

pub struct FutexData {
    data: SpinLock<HashMap<FutexKey, FutexHashBucket>>,
}

impl FutexData {
    pub fn futex_map() -> SpinLockGuard<'static, HashMap<FutexKey, FutexHashBucket>> {
        unsafe { FUTEX_DATA.as_ref().unwrap().data.lock() }
    }

    pub fn try_remove(key: &FutexKey) -> Option<FutexHashBucket> {
        unsafe {
            let mut guard = FUTEX_DATA.as_ref().unwrap().data.lock();
            if let Some(futex) = guard.get(key) {
                if futex.chain.is_empty() {
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
    chain: LinkedList<Arc<FutexObj>>,
}

impl FutexHashBucket {
    /// ## 判断是否在bucket里
    pub fn contains(&self, futex_q: &FutexObj) -> bool {
        self.chain
            .iter()
            .filter(|x| futex_q.pcb.ptr_eq(&x.pcb) && x.key == futex_q.key)
            .count()
            != 0
    }

    /// 让futex_q在该bucket上挂起
    ///
    /// 进入该函数前，需要关中断
    #[inline(always)]
    pub fn sleep_no_sched(&mut self, futex_q: Arc<FutexObj>) -> Result<(), SystemError> {
        assert!(!CurrentIrqArch::is_irq_enabled());
        self.chain.push_back(futex_q);

        ProcessManager::mark_sleep(true)?;

        Ok(())
    }

    /// ## 唤醒队列中的最多nr_wake个进程
    ///
    /// return: 唤醒的进程数
    #[inline(always)]
    pub fn wake_up(
        &mut self,
        key: FutexKey,
        bitset: Option<u32>,
        nr_wake: u32,
    ) -> Result<usize, SystemError> {
        let mut count = 0;
        let mut pop_count = 0;
        while let Some(futex_q) = self.chain.pop_front() {
            if futex_q.key == key {
                // TODO: 考虑优先级继承的机制

                if let Some(bitset) = bitset {
                    if futex_q.bitset != bitset {
                        self.chain.push_back(futex_q);
                        continue;
                    }
                }

                // 唤醒
                if futex_q.pcb.upgrade().is_some() {
                    self.remove(futex_q.clone());
                    ProcessManager::wakeup(&futex_q.pcb.upgrade().unwrap())?;
                }

                // 判断唤醒数
                count += 1;
                if count >= nr_wake {
                    break;
                }
            } else {
                self.chain.push_back(futex_q);
            }
            // 判断是否循环完队列了
            pop_count += 1;
            if pop_count >= self.chain.len() {
                break;
            }
        }
        Ok(count as usize)
    }

    /// 将FutexObj从bucket中删除
    pub fn remove(&mut self, futex: Arc<FutexObj>) {
        self.chain
            .extract_if(|x| Arc::ptr_eq(x, &futex))
            .for_each(drop);
    }
}

#[derive(Debug)]
pub struct FutexObj {
    pcb: Weak<ProcessControlBlock>,
    key: FutexKey,
    bitset: u32,
    // TODO: 优先级继承
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

/// 不同进程间通过文件共享futex变量，表明该变量在文件中的位置
#[derive(Hash, PartialEq, Eq, Clone, Debug)]
pub struct SharedKey {
    i_seq: u64,
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
                data: SpinLock::new(HashMap::new()),
            })
        };
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
                let bucket = FutexHashBucket {
                    chain: LinkedList::new(),
                };
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

        let pcb = ProcessManager::current_pcb();
        // 创建超时计时器任务
        let mut timer = None;
        if let Some(time) = abs_time {
            let wakeup_helper = WakeUpHelper::new(pcb.clone());

            let sec = time.tv_sec;
            let nsec = time.tv_nsec;
            let jiffies = next_n_us_timer_jiffies((nsec / 1000 + sec * 1_000_000) as u64);

            let wake_up = Timer::new(wakeup_helper, jiffies);

            wake_up.activate();
            timer = Some(wake_up);
        }

        let futex_q = Arc::new(FutexObj {
            pcb: Arc::downgrade(&pcb),
            key: key.clone(),
            bitset,
        });
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        // 满足条件则将当前进程在该bucket上挂起
        bucket_mut.sleep_no_sched(futex_q.clone()).map_err(|e| {
            warn!("error:{e:?}");
            e
        })?;
        drop(futex_map_guard);
        drop(irq_guard);
        schedule(SchedMode::SM_NONE);

        // 被唤醒后的检查
        let mut futex_map_guard = FutexData::futex_map();
        let bucket = futex_map_guard.get_mut(&key);
        let bucket_mut = match bucket {
            // 如果该pcb不在链表里面了或者该链表已经被释放，就证明是正常的Wake操作
            Some(bucket_mut) => {
                if !bucket_mut.contains(&futex_q) {
                    // 取消定时器任务
                    if let Some(timer) = timer {
                        timer.cancel();
                    }
                    return Ok(0);
                }
                // 非正常唤醒，返回交给下层
                bucket_mut
            }
            None => {
                // 取消定时器任务
                if let Some(timer) = timer {
                    timer.cancel();
                }
                return Ok(0);
            }
        };

        // 如果是超时唤醒，则返回错误
        if timer.is_some() && timer.clone().unwrap().timeout() {
            bucket_mut.remove(futex_q);

            return Err(SystemError::ETIMEDOUT);
        }

        // TODO: 如果没有挂起的信号，则重新判断是否满足wait要求，重新进入wait

        // 经过前面的几个判断，到这里之后，
        // 当前进程被唤醒大概率是其他进程更改了uval,需要重新去判断当前进程是否满足wait

        // 到这里之后，前面的唤醒条件都不满足，则是被信号唤醒
        // 需要处理信号然后重启futex系统调用

        // 取消定时器任务
        if let Some(timer) = timer {
            if !timer.timeout() {
                timer.cancel();
            }
        }

        Ok(0)
    }

    // ### 唤醒指定futex上挂起的最多nr_wake个进程
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
        let bucket_mut = binding.get_mut(&key).ok_or(SystemError::EINVAL)?;

        // 确保后面的唤醒操作是有意义的
        if bucket_mut.chain.is_empty() {
            return Ok(0);
        }
        // 从队列中唤醒
        let count = bucket_mut.wake_up(key.clone(), Some(bitset), nr_wake)?;

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
            if *curval != cmpval.unwrap() {
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
        let bucket1 = futex_data_guard.get_mut(&key1).ok_or(SystemError::EINVAL)?;
        let mut wake_count = 0;

        // 唤醒uaddr1中的进程
        wake_count += bucket1.wake_up(key1, None, nr_wake as u32)?;

        match Self::futex_atomic_op_inuser(op as u32, uaddr2) {
            Ok(ret) => {
                // 操作成功则唤醒uaddr2中的进程
                if ret {
                    let bucket2 = futex_data_guard.get_mut(&key2).ok_or(SystemError::EINVAL)?;
                    wake_count += bucket2.wake_up(key2, None, nr_wake2 as u32)?;
                }
            }
            Err(e) => {
                // TODO:retry?
                return Err(e);
            }
        }

        Ok(wake_count)
    }

    fn get_futex_key(
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

        // 若不是进程间共享的futex，则返回Private
        if !fshared {
            return Ok(FutexKey {
                ptr: 0,
                word: 0,
                offset: offset as u32,
                key: InnerFutexKey::Private(PrivateKey {
                    address: address as u64,
                    address_space: None,
                }),
            });
        }

        // 获取到地址所在地址空间
        let address_space = AddressSpace::current()?;
        // TODO： 判断是否为匿名映射，是匿名映射才返回PrivateKey
        return Ok(FutexKey {
            ptr: 0,
            word: 0,
            offset: offset as u32,
            key: InnerFutexKey::Private(PrivateKey {
                address: address as u64,
                address_space: Some(Arc::downgrade(&address_space)),
            }),
        });

        // 未实现共享内存机制,贡献内存部分应该通过inode构建SharedKey
        // todo!("Shared memory not implemented");
    }

    pub fn futex_atomic_op_inuser(encoded_op: u32, uaddr: VirtAddr) -> Result<bool, SystemError> {
        let op = FutexOP::from_bits((encoded_op & 0x70000000) >> 28).ok_or(SystemError::ENOSYS)?;
        let cmp =
            FutexOpCMP::from_bits((encoded_op & 0x0f000000) >> 24).ok_or(SystemError::ENOSYS)?;

        let sign_extend32 = |value: u32, index: i32| {
            let shift = (31 - index) as u8;
            return (value << shift) >> shift;
        };

        let mut oparg = sign_extend32((encoded_op & 0x00fff000) >> 12, 11);
        let cmparg = sign_extend32(encoded_op & 0x00000fff, 11);

        if (encoded_op & (FutexOP::FUTEX_OP_OPARG_SHIFT.bits() << 28) != 0) && oparg > 31 {
            warn!(
                "futex_wake_op: pid:{} tries to shift op by {}; fix this program",
                ProcessManager::current_pcb().pid().data(),
                oparg
            );

            oparg &= 31;
        }

        // TODO: 这个汇编似乎是有问题的，目前不好测试
        let old_val = Self::arch_futex_atomic_op_inuser(op, oparg, uaddr)?;

        match cmp {
            FutexOpCMP::FUTEX_OP_CMP_EQ => {
                return Ok(cmparg == old_val);
            }
            FutexOpCMP::FUTEX_OP_CMP_NE => {
                return Ok(cmparg != old_val);
            }
            FutexOpCMP::FUTEX_OP_CMP_LT => {
                return Ok(cmparg < old_val);
            }
            FutexOpCMP::FUTEX_OP_CMP_LE => {
                return Ok(cmparg <= old_val);
            }
            FutexOpCMP::FUTEX_OP_CMP_GE => {
                return Ok(cmparg >= old_val);
            }
            FutexOpCMP::FUTEX_OP_CMP_GT => {
                return Ok(cmparg > old_val);
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
        let guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        let reader =
            UserBufferReader::new(uaddr.as_ptr::<u32>(), core::mem::size_of::<u32>(), true)?;

        let oldval = reader.read_one_from_user::<u32>(0)?;

        let atomic_addr = AtomicU64::new(uaddr.data() as u64);
        // 这个指针是指向指针的指针
        let ptr = atomic_addr.as_ptr();
        match op {
            FutexOP::FUTEX_OP_SET => unsafe {
                *((*ptr) as *mut u32) = oparg;
            },
            FutexOP::FUTEX_OP_ADD => unsafe {
                *((*ptr) as *mut u32) += oparg;
            },
            FutexOP::FUTEX_OP_OR => unsafe {
                *((*ptr) as *mut u32) |= oparg;
            },
            FutexOP::FUTEX_OP_ANDN => unsafe {
                *((*ptr) as *mut u32) &= oparg;
            },
            FutexOP::FUTEX_OP_XOR => unsafe {
                *((*ptr) as *mut u32) ^= oparg;
            },
            _ => return Err(SystemError::ENOSYS),
        }

        drop(guard);

        Ok(*oldval)
    }
}

//用于指示在处理robust list是最多处理多少个条目
const ROBUST_LIST_LIMIT: isize = 2048;

#[derive(Debug, Copy, Clone)]
pub struct RobustList {
    next: VirtAddr,
}

#[derive(Debug, Copy, Clone)]
pub struct RobustListHead {
    list: RobustList,
    futex_offset: isize,
    list_op_pending: VirtAddr,
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
        let robust_list_head_len = mem::size_of::<&RobustListHead>();
        if unlikely(len != robust_list_head_len) {
            return Err(SystemError::EINVAL);
        }

        let user_buffer_reader = UserBufferReader::new(
            head_uaddr.as_ptr::<RobustListHead>(),
            mem::size_of::<RobustListHead>(),
            true,
        )?;
        let robust_list_head = *user_buffer_reader.read_one_from_user::<RobustListHead>(0)?;

        // 向内核注册robust list
        ProcessManager::current_pcb().set_robust_list(Some(robust_list_head));

        return Ok(0);
    }

    /// # 获取robust list head到用户空间
    /// ## 参数
    /// - pid：当前进程/线程的pid
    /// -
    pub fn get_robust_list(
        pid: usize,
        head_uaddr: VirtAddr,
        len_ptr_uaddr: VirtAddr,
    ) -> Result<usize, SystemError> {
        // 获取当前进程的process control block
        let pcb: Arc<ProcessControlBlock> = if pid == 0 {
            ProcessManager::current_pcb()
        } else {
            ProcessManager::find(Pid::new(pid)).ok_or(SystemError::ESRCH)?
        };

        // TODO: 检查当前进程是否能ptrace另一个进程
        let ptrace = true;
        if !ptrace {
            return Err(SystemError::EPERM);
        }

        //获取当前线程的robust list head 和 长度
        let robust_list_head = (*pcb.get_robust_list()).ok_or(SystemError::EINVAL)?;

        // 将len拷贝到用户空间len_ptr
        let mut user_writer = UserBufferWriter::new(
            len_ptr_uaddr.as_ptr::<usize>(),
            core::mem::size_of::<usize>(),
            true,
        )?;
        user_writer.copy_one_to_user(&mem::size_of::<RobustListHead>(), 0)?;
        // 将head拷贝到用户空间head
        let mut user_writer = UserBufferWriter::new(
            head_uaddr.as_ptr::<RobustListHead>(),
            mem::size_of::<RobustListHead>(),
            true,
        )?;
        user_writer.copy_one_to_user(&robust_list_head, 0)?;

        return Ok(0);
    }

    /// # 进程/线程退出时清理工作
    /// ## 参数
    /// - current：当前进程/线程的pcb
    /// - pid：当前进程/线程的pid
    pub fn exit_robust_list(pcb: Arc<ProcessControlBlock>) {
        //指向当前进程的robust list头部的指针
        let head = match *pcb.get_robust_list() {
            Some(rl) => rl,
            None => {
                return;
            }
        };
        // 遍历当前进程/线程的robust list
        for futex_uaddr in head.futexes() {
            let ret = Self::handle_futex_death(futex_uaddr, pcb.pid().into() as u32);
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

    /// # 处理进程即将死亡时，进程已经持有的futex，唤醒其他等待该futex的线程
    /// ## 参数
    /// - futex_uaddr：futex的用户空间地址
    /// - pid: 当前进程/线程的pid
    fn handle_futex_death(futex_uaddr: VirtAddr, pid: u32) -> Result<usize, SystemError> {
        let futex_val = {
            if futex_uaddr.is_null() {
                return Err(SystemError::EINVAL);
            }
            let user_buffer_reader = UserBufferReader::new(
                futex_uaddr.as_ptr::<u32>(),
                core::mem::size_of::<u32>(),
                true,
            )?;
            *user_buffer_reader.read_one_from_user::<u32>(0)?
        };

        let mut uval = futex_val;
        loop {
            // 该futex可能被其他进程占有
            let owner = uval & FUTEX_TID_MASK;
            if owner != pid {
                break;
            }

            // 判断是否有FUTEX_WAITERS和标记FUTEX_OWNER_DIED
            let mval = (uval & FUTEX_WAITERS) | FUTEX_OWNER_DIED;

            // 读取用户空间目前的futex字段，判断在此之前futex是否有被修改
            let user_buffer_reader = UserBufferReader::new(
                futex_uaddr.as_ptr::<u32>(),
                core::mem::size_of::<u32>(),
                true,
            )?;
            let nval = *user_buffer_reader.read_one_from_user::<u32>(0)?;
            if nval != mval {
                uval = nval;
                let mut user_buffer_writer = UserBufferWriter::new(
                    futex_uaddr.as_ptr::<u32>(),
                    core::mem::size_of::<u32>(),
                    true,
                )?;
                user_buffer_writer.copy_one_to_user(&mval, 0)?;
                continue;
            }

            // 有等待者,进行唤醒操作
            if nval & FUTEX_WAITERS != 0 {
                let mut flags = FutexFlag::FLAGS_MATCH_NONE;
                flags.insert(FutexFlag::FLAGS_SHARED);
                Futex::futex_wake(futex_uaddr, flags, 1, FUTEX_BITSET_MATCH_ANY)?;
            }
            break;
        }

        return Ok(0);
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
}

impl Iterator for FutexIterator<'_> {
    type Item = VirtAddr;

    fn next(&mut self) -> Option<Self::Item> {
        if self.is_end() {
            return None;
        }

        while self.entry.data() != &self.robust_list_head.list as *const RobustList as usize {
            if self.count == ROBUST_LIST_LIMIT {
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

            let user_buffer_reader = UserBufferReader::new(
                self.entry.as_ptr::<RobustList>(),
                mem::size_of::<RobustList>(),
                true,
            )
            .ok()?;
            let next_entry = user_buffer_reader
                .read_one_from_user::<RobustList>(0)
                .ok()?;

            self.entry = next_entry.next;

            self.count += 1;

            if futex_uaddr.is_some() {
                return futex_uaddr;
            }
        }
        self.count -= 1;
        self.robust_list_head.pending_uaddr()
    }
}
