#![allow(dead_code)]
use core::{
    cell::UnsafeCell,
    hint::spin_loop,
    mem::{self, ManuallyDrop},
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU32, Ordering},
};

use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::{InterruptArch, IrqFlagsGuard},
    process::ProcessManager,
};

///RwLock读写锁

/// @brief READER位占据从右往左数第三个比特位
const READER: u32 = 1 << 2;

/// @brief UPGRADED位占据从右到左数第二个比特位
const UPGRADED: u32 = 1 << 1;

/// @brief WRITER位占据最右边的比特位
const WRITER: u32 = 1;

const READER_BIT: u32 = 2;

/// @brief 读写锁的基本数据结构
/// @param lock 32位原子变量,最右边的两位从左到右分别是UPGRADED,WRITER (标志位)
///             剩下的bit位存储READER数量(除了MSB)
///             对于标志位,0代表无, 1代表有
///             对于剩下的比特位表征READER的数量的多少
///             lock的MSB必须为0,否则溢出
#[derive(Debug)]
pub struct RwLock<T> {
    lock: AtomicU32,
    data: UnsafeCell<T>,
}

/// @brief  READER守卫的数据结构
/// @param lock 是对RwLock的lock属性值的只读引用
pub struct RwLockReadGuard<'a, T: 'a> {
    data: *const T,
    lock: &'a AtomicU32,
    irq_guard: Option<IrqFlagsGuard>,
}

/// @brief UPGRADED是介于READER和WRITER之间的一种锁,它可以升级为WRITER,
///        UPGRADED守卫的数据结构,注册UPGRADED锁只需要查看UPGRADED和WRITER的比特位
///        但是当UPGRADED守卫注册后,不允许有新的读者锁注册
/// @param inner    是对RwLock数据结构的只读引用
pub struct RwLockUpgradableGuard<'a, T: 'a> {
    data: *const T,
    inner: &'a RwLock<T>,
    irq_guard: Option<IrqFlagsGuard>,
}

/// @brief WRITER守卫的数据结构
/// @param data     RwLock的data的可变引用
/// @param inner    是对RwLock数据结构的只读引用    
pub struct RwLockWriteGuard<'a, T: 'a> {
    data: *mut T,
    inner: &'a RwLock<T>,
    irq_guard: Option<IrqFlagsGuard>,
}

unsafe impl<T: Send> Send for RwLock<T> {}
unsafe impl<T: Send + Sync> Sync for RwLock<T> {}

/// @brief RwLock的API
impl<T> RwLock<T> {
    #[inline]
    /// @brief  RwLock的初始化
    pub const fn new(data: T) -> Self {
        return RwLock {
            lock: AtomicU32::new(0),
            data: UnsafeCell::new(data),
        };
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 将读写锁的皮扒掉,返回内在的data,返回的是一个真身而非引用
    pub fn into_inner(self) -> T {
        let RwLock { data, .. } = self;
        return data.into_inner();
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 返回data的raw pointer,
    /// unsafe
    pub fn as_mut_ptr(&self) -> *mut T {
        return self.data.get();
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 获取实时的读者数并尝试加1,如果增加值成功则返回增加1后的读者数,否则panic
    fn current_reader(&self) -> Result<u32, SystemError> {
        const MAX_READERS: u32 = core::u32::MAX >> READER_BIT >> 1; //右移3位

        let value = self.lock.fetch_add(READER, Ordering::Acquire);
        //value二进制形式的MSB不能为1, 否则导致溢出

        if value > MAX_READERS << READER_BIT {
            self.lock.fetch_sub(READER, Ordering::Release);
            //panic!("Too many lock readers, cannot safely proceed");
            return Err(SystemError::EOVERFLOW);
        } else {
            return Ok(value);
        }
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 尝试获取READER守卫
    pub fn try_read(&self) -> Option<RwLockReadGuard<T>> {
        ProcessManager::preempt_disable();
        let r = self.inner_try_read();
        if r.is_none() {
            ProcessManager::preempt_enable();
        }
        return r;
    }

    fn inner_try_read(&self) -> Option<RwLockReadGuard<T>> {
        let reader_value = self.current_reader();
        //得到自增后的reader_value, 包括了尝试获得READER守卫的进程
        let value;

        if reader_value.is_err() {
            return None; //获取失败
        } else {
            value = reader_value.unwrap();
        }

        //判断有没有writer和upgrader
        //注意, 若upgrader存在,已经存在的读者继续占有锁,但新读者不允许获得锁
        if value & (WRITER | UPGRADED) != 0 {
            self.lock.fetch_sub(READER, Ordering::Release);
            return None;
        } else {
            return Some(RwLockReadGuard {
                data: unsafe { &*self.data.get() },
                lock: &self.lock,
                irq_guard: None,
            });
        }
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 获得READER的守卫
    pub fn read(&self) -> RwLockReadGuard<T> {
        loop {
            match self.try_read() {
                Some(guard) => return guard,
                None => spin_loop(),
            }
        } //忙等待
    }

    /// 关中断并获取读者守卫
    pub fn read_irqsave(&self) -> RwLockReadGuard<T> {
        loop {
            let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            match self.try_read() {
                Some(mut guard) => {
                    guard.irq_guard = Some(irq_guard);
                    return guard;
                }
                None => spin_loop(),
            }
        }
    }

    /// 尝试关闭中断并获取读者守卫
    pub fn try_read_irqsave(&self) -> Option<RwLockReadGuard<T>> {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        if let Some(mut guard) = self.try_read() {
            guard.irq_guard = Some(irq_guard);
            return Some(guard);
        } else {
            return None;
        }
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 获取读者+UPGRADER的数量, 不能保证能否获得同步值
    pub fn reader_count(&self) -> u32 {
        let state = self.lock.load(Ordering::Relaxed);
        return state / READER + (state & UPGRADED) / UPGRADED;
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 获取写者数量,不能保证能否获得同步值
    pub fn writer_count(&self) -> u32 {
        return (self.lock.load(Ordering::Relaxed) & WRITER) / WRITER;
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    #[allow(dead_code)]
    #[inline]
    /// @brief 尝试获得WRITER守卫
    pub fn try_write(&self) -> Option<RwLockWriteGuard<T>> {
        ProcessManager::preempt_disable();
        let r = self.inner_try_write();
        if r.is_none() {
            ProcessManager::preempt_enable();
        }

        return r;
    } //当架构为arm时,有些代码需要作出调整compare_exchange=>compare_exchange_weak

    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    #[allow(dead_code)]
    #[inline]
    pub fn try_write_irqsave(&self) -> Option<RwLockWriteGuard<T>> {
        ProcessManager::preempt_disable();
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let r = self.inner_try_write().map(|mut g| {
            g.irq_guard = Some(irq_guard);
            g
        });
        if r.is_none() {
            ProcessManager::preempt_enable();
        }

        return r;
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    #[allow(dead_code)]
    fn inner_try_write(&self) -> Option<RwLockWriteGuard<T>> {
        let res: bool = self
            .lock
            .compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();
        //只有lock大小为0的时候能获得写者守卫
        if res {
            return Some(RwLockWriteGuard {
                data: unsafe { &mut *self.data.get() },
                inner: self,
                irq_guard: None,
            });
        } else {
            return None;
        }
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 获得WRITER守卫
    pub fn write(&self) -> RwLockWriteGuard<T> {
        loop {
            match self.try_write() {
                Some(guard) => return guard,
                None => spin_loop(),
            }
        }
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 获取WRITER守卫并关中断
    pub fn write_irqsave(&self) -> RwLockWriteGuard<T> {
        loop {
            let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            match self.try_write() {
                Some(mut guard) => {
                    guard.irq_guard = Some(irq_guard);
                    return guard;
                }
                None => spin_loop(),
            }
        }
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 尝试获得UPGRADER守卫
    pub fn try_upgradeable_read(&self) -> Option<RwLockUpgradableGuard<T>> {
        ProcessManager::preempt_disable();
        let r = self.inner_try_upgradeable_read();
        if r.is_none() {
            ProcessManager::preempt_enable();
        }

        return r;
    }

    #[allow(dead_code)]
    pub fn try_upgradeable_read_irqsave(&self) -> Option<RwLockUpgradableGuard<T>> {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::preempt_disable();
        let mut r = self.inner_try_upgradeable_read();
        if r.is_none() {
            ProcessManager::preempt_enable();
        } else {
            r.as_mut().unwrap().irq_guard = Some(irq_guard);
        }

        return r;
    }

    fn inner_try_upgradeable_read(&self) -> Option<RwLockUpgradableGuard<T>> {
        // 获得UPGRADER守卫不需要查看读者位
        // 如果获得读者锁失败,不需要撤回fetch_or的原子操作
        if self.lock.fetch_or(UPGRADED, Ordering::Acquire) & (WRITER | UPGRADED) == 0 {
            return Some(RwLockUpgradableGuard {
                inner: self,
                data: unsafe { &mut *self.data.get() },
                irq_guard: None,
            });
        } else {
            return None;
        }
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 获得UPGRADER守卫
    pub fn upgradeable_read(&self) -> RwLockUpgradableGuard<T> {
        loop {
            match self.try_upgradeable_read() {
                Some(guard) => return guard,
                None => spin_loop(),
            }
        }
    }

    #[inline]
    /// @brief 获得UPGRADER守卫
    pub fn upgradeable_read_irqsave(&self) -> RwLockUpgradableGuard<T> {
        loop {
            let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            match self.try_upgradeable_read() {
                Some(mut guard) => {
                    guard.irq_guard = Some(irq_guard);
                    return guard;
                }
                None => spin_loop(),
            }
        }
    }

    #[allow(dead_code)]
    #[inline]
    //extremely unsafe behavior
    /// @brief 强制减少READER数
    pub unsafe fn force_read_decrement(&self) {
        debug_assert!(self.lock.load(Ordering::Relaxed) & !WRITER > 0);
        self.lock.fetch_sub(READER, Ordering::Release);
    }

    #[allow(dead_code)]
    #[inline]
    //extremely unsafe behavior
    /// @brief 强制给WRITER解锁
    pub unsafe fn force_write_unlock(&self) {
        debug_assert_eq!(self.lock.load(Ordering::Relaxed) & !(WRITER | UPGRADED), 0);
        self.lock.fetch_and(!(WRITER | UPGRADED), Ordering::Release);
    }

    #[allow(dead_code)]
    pub unsafe fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.data.get() }
    }
}

impl<T: Default> Default for RwLock<T> {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

/// @brief 由原有的值创建新的锁
impl<T> From<T> for RwLock<T> {
    fn from(data: T) -> Self {
        return Self::new(data);
    }
}

impl<'rwlock, T> RwLockReadGuard<'rwlock, T> {
    /// @brief 释放守卫,获得保护的值的不可变引用
    ///
    /// ## Safety
    ///
    /// 由于这样做可能导致守卫在另一个线程中被释放，从而导致pcb的preempt count不正确，
    /// 因此必须小心的手动维护好preempt count。
    ///
    /// 并且，leak还可能导致锁的状态不正确。因此请仔细考虑是否真的需要使用这个函数。
    #[allow(dead_code)]
    #[inline]
    pub unsafe fn leak(this: Self) -> &'rwlock T {
        let this = ManuallyDrop::new(this);
        return unsafe { &*this.data };
    }
}

impl<'rwlock, T> RwLockUpgradableGuard<'rwlock, T> {
    #[allow(dead_code)]
    #[inline]
    /// @brief 尝试将UPGRADER守卫升级为WRITER守卫
    pub fn try_upgrade(mut self) -> Result<RwLockWriteGuard<'rwlock, T>, Self> {
        let res = self.inner.lock.compare_exchange(
            UPGRADED,
            WRITER,
            Ordering::Acquire,
            Ordering::Relaxed,
        );
        //当且仅当只有UPGRADED守卫时可以升级

        if res.is_ok() {
            let inner = self.inner;
            let irq_guard = self.irq_guard.take();
            mem::forget(self);

            Ok(RwLockWriteGuard {
                data: unsafe { &mut *inner.data.get() },
                inner,
                irq_guard,
            })
        } else {
            Err(self)
        }
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 将upgrader升级成writer
    pub fn upgrade(mut self) -> RwLockWriteGuard<'rwlock, T> {
        loop {
            self = match self.try_upgrade() {
                Ok(writeguard) => return writeguard,
                Err(former) => former,
            };

            spin_loop();
        }
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief UPGRADER降级为READER
    pub fn downgrade(mut self) -> RwLockReadGuard<'rwlock, T> {
        while self.inner.current_reader().is_err() {
            spin_loop();
        }

        let inner: &RwLock<T> = self.inner;
        let irq_guard = self.irq_guard.take();
        // 自动移去UPGRADED比特位
        mem::drop(self);

        RwLockReadGuard {
            data: unsafe { &*inner.data.get() },
            lock: &inner.lock,
            irq_guard,
        }
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 返回内部数据的引用,消除守卫
    ///
    /// ## Safety
    ///
    /// 由于这样做可能导致守卫在另一个线程中被释放，从而导致pcb的preempt count不正确，
    /// 因此必须小心的手动维护好preempt count。
    ///
    /// 并且，leak还可能导致锁的状态不正确。因此请仔细考虑是否真的需要使用这个函数。
    pub unsafe fn leak(this: Self) -> &'rwlock T {
        let this: ManuallyDrop<RwLockUpgradableGuard<'_, T>> = ManuallyDrop::new(this);

        unsafe { &*this.data }
    }
}

impl<'rwlock, T> RwLockWriteGuard<'rwlock, T> {
    #[allow(dead_code)]
    #[inline]
    /// @brief 返回内部数据的引用,消除守卫
    ///
    /// ## Safety
    ///
    /// 由于这样做可能导致守卫在另一个线程中被释放，从而导致pcb的preempt count不正确，
    /// 因此必须小心的手动维护好preempt count。
    ///
    /// 并且，leak还可能导致锁的状态不正确。因此请仔细考虑是否真的需要使用这个函数。
    pub unsafe fn leak(this: Self) -> &'rwlock T {
        let this = ManuallyDrop::new(this);

        return unsafe { &*this.data };
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 将WRITER降级为READER
    pub fn downgrade(mut self) -> RwLockReadGuard<'rwlock, T> {
        while self.inner.current_reader().is_err() {
            spin_loop();
        }
        //本质上来说绝对保证没有任何读者

        let inner = self.inner;
        let irq_guard = self.irq_guard.take();
        mem::drop(self);

        return RwLockReadGuard {
            data: unsafe { &*inner.data.get() },
            lock: &inner.lock,
            irq_guard,
        };
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 将WRITER降级为UPGRADER
    pub fn downgrade_to_upgradeable(mut self) -> RwLockUpgradableGuard<'rwlock, T> {
        debug_assert_eq!(
            self.inner.lock.load(Ordering::Acquire) & (WRITER | UPGRADED),
            WRITER
        );

        self.inner.lock.store(UPGRADED, Ordering::Release);

        let inner = self.inner;

        let irq_guard = self.irq_guard.take();
        mem::forget(self);

        return RwLockUpgradableGuard {
            inner,
            data: unsafe { &*inner.data.get() },
            irq_guard,
        };
    }
}

impl<'rwlock, T> Deref for RwLockReadGuard<'rwlock, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        return unsafe { &*self.data };
    }
}

impl<'rwlock, T> Deref for RwLockUpgradableGuard<'rwlock, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        return unsafe { &*self.data };
    }
}

impl<'rwlock, T> Deref for RwLockWriteGuard<'rwlock, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        return unsafe { &*self.data };
    }
}

impl<'rwlock, T> DerefMut for RwLockWriteGuard<'rwlock, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        return unsafe { &mut *self.data };
    }
}

impl<'rwlock, T> Drop for RwLockReadGuard<'rwlock, T> {
    fn drop(&mut self) {
        debug_assert!(self.lock.load(Ordering::Relaxed) & !(WRITER | UPGRADED) > 0);
        self.lock.fetch_sub(READER, Ordering::Release);
        ProcessManager::preempt_enable();
    }
}

impl<'rwlock, T> Drop for RwLockUpgradableGuard<'rwlock, T> {
    fn drop(&mut self) {
        debug_assert_eq!(
            self.inner.lock.load(Ordering::Relaxed) & (WRITER | UPGRADED),
            UPGRADED
        );
        self.inner.lock.fetch_sub(UPGRADED, Ordering::AcqRel);
        ProcessManager::preempt_enable();
        //这里为啥要AcqRel? Release应该就行了?
    }
}

impl<'rwlock, T> Drop for RwLockWriteGuard<'rwlock, T> {
    fn drop(&mut self) {
        debug_assert_eq!(self.inner.lock.load(Ordering::Relaxed) & WRITER, WRITER);
        self.inner
            .lock
            .fetch_and(!(WRITER | UPGRADED), Ordering::Release);
        self.irq_guard.take();
        ProcessManager::preempt_enable();
    }
}
