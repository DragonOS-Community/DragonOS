use core::intrinsics::likely;

use alloc::{collections::LinkedList, sync::Arc};
use hashbrown::HashMap;

use crate::{
    arch::{sched::sched, CurrentIrqArch, MMArch},
    exception::InterruptArch,
    include::bindings::bindings::verify_area,
    libs::spinlock::SpinLock,
    mm::MemoryManagementArch,
    process::{ProcessControlBlock, ProcessManager},
    syscall::{user_access::UserBufferReader, SystemError},
    time::{
        timer::{next_n_us_timer_jiffies, Timer, WakeUpHelper},
        TimeSpec,
    },
};

use super::constant::FLAGS_SHARED;

lazy_static! {
    pub(super) static ref FUTEX_DATA: SpinLock<HashMap<FutexKey, LockedFutexHashBucket>> =
        SpinLock::new(HashMap::new());
}

pub struct Futex;

pub struct LockedFutexHashBucket(SpinLock<FutexHashBucket>);

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
            .filter(|x| x.pcb.pid() == futex_q.pcb.pid() && x.key == futex_q.key)
            .count()
            != 0
    }

    /// 让futex_q在该bucket上挂起
    #[inline(always)]
    pub fn sleep(&mut self, futex_q: Arc<FutexObj>) -> Result<(), SystemError> {
        self.chain.push_back(futex_q);

        // 关中断并且标记睡眠
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(true)?;
        drop(irq_guard);

        sched();

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
                ProcessManager::wakeup(&futex_q.pcb)?;

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
}

pub struct FutexObj {
    pcb: Arc<ProcessControlBlock>,
    key: FutexKey,
    bitset: u32,
    // TODO: 优先级继承
}

pub enum FutexAccess {
    FutexRead,
    FutexWrite,
}

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
/// ### 用于定位内核唯一的futex
pub enum InnerFutexKey {
    Shared(SharedKey),
    Private(PrivateKey),
}

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
pub struct FutexKey {
    ptr: u64,
    word: u64,
    offset: u32,
    key: InnerFutexKey,
}

/// 不同进程间通过文件共享futex变量，表明该变量在文件中的位置
#[derive(Hash, PartialEq, Eq, Clone, Copy)]
pub struct SharedKey {
    i_seq: u64,
    page_offset: u64,
}

/// 同一进程的不同线程共享futex变量，表明该变量在进程地址空间中的位置
#[derive(Hash, PartialEq, Eq, Clone, Copy)]
pub struct PrivateKey {
    // 表示所在页面的初始地址
    address: u64,
}

impl Futex {
    pub fn futex_wait(
        uaddr: *const u32,
        flags: u32,
        val: u32,
        abs_time: *const TimeSpec,
        bitset: u32,
    ) -> Result<usize, SystemError> {
        if bitset == 0 {
            return Err(SystemError::EINVAL);
        }

        // 获取全局hash表的key值
        let key = Self::get_futex_key(uaddr, flags & FLAGS_SHARED != 0, FutexAccess::FutexRead)?;
        let mut binding = FUTEX_DATA.lock();
        let bucket = binding.get(&key);
        let bucket = match bucket {
            Some(bucket) => bucket,
            None => {
                let bucket = LockedFutexHashBucket(SpinLock::new(FutexHashBucket {
                    chain: LinkedList::new(),
                }));
                binding.insert(key, bucket);
                binding.get(&key).unwrap()
            }
        };

        // 使用UserBuffer读取futex
        let user_reader = UserBufferReader::new(uaddr, 4, true)?;

        // 从用户空间读取到futex的val
        let mut uval = 0;

        // 对bucket上锁，避免在读之前futex值被更改
        let mut bucket_guard = bucket.0.lock();

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
        if !abs_time.is_null() {
            // 校验地址
            let buffer_reader =
                UserBufferReader::new(abs_time, core::mem::size_of::<TimeSpec>(), true)?;

            let time = buffer_reader.read_one_from_user::<TimeSpec>(0)?;

            let wakeup_helper = WakeUpHelper::new(pcb.clone());

            let sec = time.tv_sec;
            let nsec = time.tv_nsec;
            let jiffies = next_n_us_timer_jiffies((nsec / 1000 + sec * 1_000_000) as u64);

            let wake_up = Timer::new(wakeup_helper, jiffies);

            wake_up.activate();
            timer = Some(wake_up);
        }

        let futex_q = Arc::new(FutexObj { pcb, key, bitset });

        // 满足条件则将当前进程在该bucket上挂起
        bucket_guard.sleep(futex_q.clone())?;

        // 被唤醒后的检查

        // 如果该pcb不在链表里面了，就证明是正常的Wake操作
        if !bucket_guard.contains(&futex_q) {
            // 取消定时器任务
            if timer.is_some() {
                timer.unwrap().cancel();
            }

            return Ok(0);
        }

        // 如果是超时唤醒，则返回错误
        if timer.is_some() {
            if timer.clone().unwrap().timeout() {
                return Err(SystemError::ETIMEDOUT);
            }
        }

        // TODO: 如果没有挂起的信号，则重新判断是否满足wait要求，重新进入wait

        // 经过前面的几个判断，到这里之后，
        // 当前进程被唤醒大概率是其他进程更改了uval,需要重新去判断当前进程是否满足wait

        // 到这里之后，前面的唤醒条件都不满足，则是被信号唤醒
        // 需要处理信号然后重启futex系统调用

        // 取消定时器任务
        if timer.is_some() {
            let timer = timer.unwrap();
            if !timer.timeout() {
                timer.cancel();
            }
        }
        Ok(0)
    }

    pub fn futex_wake(
        uaddr: *const u32,
        flags: u32,
        nr_wake: u32,
        bitset: u32,
    ) -> Result<usize, SystemError> {
        if bitset == 0 {
            return Err(SystemError::EINVAL);
        }

        // 获取futex_key,并且判断地址空间合法性
        let key = Self::get_futex_key(uaddr, flags & FLAGS_SHARED != 0, FutexAccess::FutexRead)?;
        let binding = FUTEX_DATA.lock();
        let bucket = binding.get(&key).ok_or(SystemError::EINVAL)?;

        let mut bucket_guard = bucket.0.lock();

        // 确保后面的唤醒操作是有意义的
        if bucket_guard.chain.len() == 0 {
            return Ok(0);
        }

        // 从队列中唤醒
        Ok(bucket_guard.wake_up(key, Some(bitset), nr_wake)?)
    }

    pub fn futex_requeue(
        uaddr1: *const u32,
        flags: u32,
        uaddr2: *const u32,
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

        let key1 = Self::get_futex_key(uaddr1, flags & FLAGS_SHARED != 0, FutexAccess::FutexRead)?;
        let key2 = Self::get_futex_key(uaddr2, flags & FLAGS_SHARED != 0, {
            match requeue_pi {
                true => FutexAccess::FutexWrite,
                false => FutexAccess::FutexRead,
            }
        })?;

        if requeue_pi && key1 == key2 {
            return Err(SystemError::EINVAL);
        }

        let binding = FUTEX_DATA.lock();
        let bucket1 = binding.get(&key1).ok_or(SystemError::EINVAL)?;
        let bucket2 = binding.get(&key2).ok_or(SystemError::EINVAL)?;

        if likely(!cmpval.is_none()) {
            let uval_reader = UserBufferReader::new(uaddr1, core::mem::size_of::<u32>(), true)?;
            let curval = uval_reader.read_one_from_user::<u32>(0)?;

            // 判断是否满足条件
            if *curval != cmpval.unwrap() {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        }

        // 对bucket上锁
        let mut bucket_guard_1 = bucket1.0.lock();
        let mut bucket_guard_2 = bucket2.0.lock();

        if !requeue_pi {
            // 唤醒nr_wake个进程
            let ret = bucket_guard_1.wake_up(key1, None, nr_wake as u32)?;

            // 将bucket1中最多nr_requeue个任务转移到bucket2
            for _ in 0..nr_requeue {
                let futex_q = bucket_guard_1.chain.pop_front();
                match futex_q {
                    Some(futex_q) => {
                        bucket_guard_2.chain.push_back(futex_q);
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

    pub fn futex_wake_op(
        uaddr1: *const u32,
        flags: u32,
        uaddr2: *const u32,
        nr_wake: i32,
        nr_wake2: i32,
        op: i32,
    ) -> Result<usize, SystemError> {
        let key1 = Futex::get_futex_key(uaddr1, flags & FLAGS_SHARED != 0, FutexAccess::FutexRead)?;
        let key2 =
            Futex::get_futex_key(uaddr2, flags & FLAGS_SHARED != 0, FutexAccess::FutexWrite)?;

        let binding = FUTEX_DATA.lock();
        let bucket1 = binding.get(&key1).ok_or(SystemError::EINVAL)?;
        let bucket2 = binding.get(&key2).ok_or(SystemError::EINVAL)?;

        Ok(0)
    }

    fn get_futex_key(
        uaddr: *const u32,
        fshared: bool,
        access: FutexAccess,
    ) -> Result<FutexKey, SystemError> {
        let mut address = uaddr as u64;

        // 计算相对页的偏移量
        let offset = address % MMArch::PAGE_SIZE as u64;
        // 判断内存对齐
        if !(uaddr as usize & (core::mem::size_of::<u32>() - 1) == 0) {
            return Err(SystemError::EINVAL);
        }

        // 目前address指向所在页面的起始地址
        address -= offset;

        // 判断地址空间的可访问性
        if !unsafe { verify_area(uaddr as u64, core::mem::size_of::<u32>() as u64) } {
            return Err(SystemError::EFAULT);
        }

        // 若不是进程间共享的futex，则返回Private
        if !fshared {
            return Ok(FutexKey {
                ptr: 0,
                word: 0,
                offset: offset as u32,
                key: InnerFutexKey::Private(PrivateKey {
                    address: address as u64,
                }),
            });
        }
        // 未实现共享内存机制
        todo!("Shared memory not implemented");
    }
}
