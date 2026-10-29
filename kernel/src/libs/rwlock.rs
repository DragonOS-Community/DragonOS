#![allow(dead_code)]
use core::{
    cell::UnsafeCell,
    hint::spin_loop,
    mem::{self, ManuallyDrop},
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::bottom_half::{local_bh_disable, LocalBhDisableGuard},
    exception::{in_interrupt, InterruptArch, IrqFlagsGuard},
    process::ProcessManager,
};

///RwLock读写锁
/// @brief READER位占据从右往左数第三个比特位
const READER: u64 = 1 << 2;

/// @brief UPGRADED位占据从右到左数第二个比特位
const UPGRADED: u64 = 1 << 1;

/// @brief WRITER位占据最右边的比特位
const WRITER: u64 = 1;

const OWNER_MASK: u64 = WRITER | UPGRADED;
const READER_MASK: u64 = (1u64 << 32) - READER;
const ACTIVE_MASK: u64 = OWNER_MASK | READER_MASK;
const PENDING_WRITER_ONE: u64 = 1u64 << 32;
const PENDING_WRITER_MASK: u64 = !((1u64 << 32) - 1);
const MAX_READERS: u64 = READER_MASK / READER;

/// @brief 读写锁的基本数据结构
/// @param lock 64位原子变量。低32位保存所有者和读者状态，高32位保存等待写者数量。
///             对于标志位,0代表无, 1代表有
///             对于剩下的比特位表征READER的数量的多少
///             lock的MSB必须为0,否则溢出
#[derive(Debug)]
pub struct RwLock<T> {
    lock: AtomicU64,
    writer_tickets: WriterTickets,
    data: UnsafeCell<T>,
}

/// FIFO admission state for blocking writers. Keeping ticket arithmetic in a
/// small production helper makes wrap-around and pending semantics testable
/// without exposing lock internals or creating a test-only control path.
#[derive(Debug)]
struct WriterTickets {
    next: AtomicU32,
    serving: AtomicU32,
}

impl WriterTickets {
    const fn new() -> Self {
        Self {
            next: AtomicU32::new(0),
            serving: AtomicU32::new(0),
        }
    }

    #[inline]
    fn issue(&self) -> u32 {
        self.next.fetch_add(1, Ordering::AcqRel)
    }

    #[inline]
    fn is_turn(&self, ticket: u32) -> bool {
        self.serving.load(Ordering::Acquire) == ticket
    }

    #[inline]
    fn finish(&self, ticket: u32) {
        debug_assert_eq!(self.serving.load(Ordering::Relaxed), ticket);
        self.serving.fetch_add(1, Ordering::Release);
    }
}

/// @brief  READER守卫的数据结构
/// @param lock 是对RwLock的lock属性值的只读引用
pub struct RwLockReadGuard<'a, T: 'a> {
    data: *const T,
    lock: &'a AtomicU64,
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

/// `*_bh` 风格的读锁守卫：持锁期间屏蔽本 CPU 的 softirq/tasklet 执行（不关硬中断）。
pub struct RwLockReadBhGuard<'a, T: 'a> {
    guard: RwLockReadGuard<'a, T>,
    bh: LocalBhDisableGuard,
}

/// `*_bh` 风格的写锁守卫：持锁期间屏蔽本 CPU 的 softirq/tasklet 执行（不关硬中断）。
pub struct RwLockWriteBhGuard<'a, T: 'a> {
    guard: RwLockWriteGuard<'a, T>,
    bh: LocalBhDisableGuard,
}

unsafe impl<T: Send> Send for RwLock<T> {}
unsafe impl<T: Send + Sync> Sync for RwLock<T> {}

/// @brief RwLock的API
impl<T> RwLock<T> {
    #[inline]
    /// @brief  RwLock的初始化
    pub const fn new(data: T) -> Self {
        return RwLock {
            lock: AtomicU64::new(0),
            writer_tickets: WriterTickets::new(),
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
    /// @brief 尝试原子注册读者；owner、reader 与 pending writer 使用同一状态快照判定。
    fn current_reader(&self, bypass_waiting_writer: bool) -> Result<(), SystemError> {
        let mut state = self.lock.load(Ordering::Relaxed);
        loop {
            if state & OWNER_MASK != 0
                || (!bypass_waiting_writer && state & PENDING_WRITER_MASK != 0)
            {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            if state & READER_MASK == MAX_READERS * READER {
                return Err(SystemError::EOVERFLOW);
            }
            state = match self.lock.compare_exchange_weak(
                state,
                state + READER,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => actual,
            };
        }
    }

    #[inline]
    fn has_waiting_writer(&self) -> bool {
        self.lock.load(Ordering::Acquire) & PENDING_WRITER_MASK != 0
    }

    /// Whether this reader executes in an actual hardirq or softirq callback.
    #[inline]
    fn interrupt_reader() -> bool {
        in_interrupt()
    }

    #[inline]
    fn register_waiting_writer(&self) -> Result<u32, SystemError> {
        // Publish pending before issuing the FIFO ticket. A stalled registrant
        // may conservatively block readers, but every issued ticket is backed
        // by an already-visible pending count. This prevents the head writer
        // from observing its turn before reader admission has been closed.
        self.lock
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |state| {
                (state & PENDING_WRITER_MASK != PENDING_WRITER_MASK)
                    .then_some(state + PENDING_WRITER_ONE)
            })
            .map_err(|_| SystemError::EOVERFLOW)?;
        Ok(self.writer_tickets.issue())
    }

    #[inline]
    fn writer_turn(&self, ticket: u32) -> bool {
        self.writer_tickets.is_turn(ticket)
    }

    #[inline]
    fn finish_writer_turn(&self, ticket: u32) {
        self.writer_tickets.finish(ticket)
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 尝试获取READER守卫
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        // Like Linux qrwlock's interrupt slowpath, an interrupt-context
        // reader must not wait behind a queued writer: the interrupted outer
        // reader may be the reader that writer is waiting for.  DragonOS uses
        // an explicit hardirq/softirq predicate; merely disabling preemption
        // does not qualify for this exception.
        ProcessManager::preempt_disable();
        let bypass_waiting_writer = Self::interrupt_reader();
        let r = self.inner_try_read(bypass_waiting_writer);
        if r.is_none() {
            ProcessManager::preempt_enable();
        }
        return r;
    }

    fn inner_try_read(&self, bypass_waiting_writer: bool) -> Option<RwLockReadGuard<'_, T>> {
        if self.current_reader(bypass_waiting_writer).is_err() {
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
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        loop {
            match self.try_read() {
                Some(guard) => return guard,
                None => spin_loop(),
            }
        } //忙等待
    }

    /// 关中断并获取读者守卫
    ///
    /// 等价于 Linux `__raw_read_lock_irqsave`：先关 IRQ 再关抢占，自旋期间两者始终关闭。
    pub fn read_irqsave(&self) -> RwLockReadGuard<'_, T> {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::preempt_disable();
        let bypass_waiting_writer = Self::interrupt_reader();
        loop {
            match self.inner_try_read(bypass_waiting_writer) {
                Some(mut guard) => {
                    guard.irq_guard = Some(irq_guard);
                    return guard;
                }
                None => spin_loop(),
            }
        }
    }

    /// `read_lock_bh()`：禁用本 CPU BH 后获取读锁。
    ///
    /// 注意：该接口不关硬中断；若该锁也会在 hardirq 获取，则必须使用 `read_irqsave()`。
    pub fn read_bh(&self) -> RwLockReadBhGuard<'_, T> {
        let bh = local_bh_disable();
        let guard = self.read();
        RwLockReadBhGuard { bh, guard }
    }

    /// 尝试关闭中断并获取读者守卫
    pub fn try_read_irqsave(&self) -> Option<RwLockReadGuard<'_, T>> {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::preempt_disable();
        let bypass_waiting_writer = Self::interrupt_reader();
        if let Some(mut guard) = self.inner_try_read(bypass_waiting_writer) {
            guard.irq_guard = Some(irq_guard);
            return Some(guard);
        } else {
            ProcessManager::preempt_enable();
            return None;
        }
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 获取读者+UPGRADER的数量, 不能保证能否获得同步值
    pub fn reader_count(&self) -> u32 {
        let state = self.lock.load(Ordering::Relaxed);
        return ((state & READER_MASK) / READER + u64::from(state & UPGRADED != 0)) as u32;
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 获取写者数量,不能保证能否获得同步值
    pub fn writer_count(&self) -> u32 {
        return u32::from(self.lock.load(Ordering::Relaxed) & WRITER != 0);
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 尝试获得WRITER守卫
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        ProcessManager::preempt_disable();
        let r = self.inner_try_write();
        if r.is_none() {
            ProcessManager::preempt_enable();
        }

        return r;
    } //当架构为arm时,有些代码需要作出调整compare_exchange=>compare_exchange_weak

    #[allow(dead_code)]
    #[inline]
    pub fn try_write_irqsave(&self) -> Option<RwLockWriteGuard<'_, T>> {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::preempt_disable();
        let r = self.inner_try_write().map(|mut g| {
            g.irq_guard = Some(irq_guard);
            g
        });
        if r.is_none() {
            ProcessManager::preempt_enable();
        }

        return r;
    }

    #[allow(dead_code)]
    fn inner_try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        self.lock
            .compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| RwLockWriteGuard {
                data: unsafe { &mut *self.data.get() },
                inner: self,
                irq_guard: None,
            })
    }

    #[inline]
    fn inner_try_write_registered(&self) -> Option<RwLockWriteGuard<'_, T>> {
        let mut state = self.lock.load(Ordering::Relaxed);
        loop {
            if state & ACTIVE_MASK != 0 || state & PENDING_WRITER_MASK == 0 {
                return None;
            }
            let new_state = (state - PENDING_WRITER_ONE) | WRITER;
            state = match self.lock.compare_exchange_weak(
                state,
                new_state,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(RwLockWriteGuard {
                        data: unsafe { &mut *self.data.get() },
                        inner: self,
                        irq_guard: None,
                    })
                }
                Err(actual) => actual,
            };
        }
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 获得WRITER守卫
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        ProcessManager::preempt_disable();
        let ticket = self
            .register_waiting_writer()
            .expect("too many pending RwLock writers");
        loop {
            if !self.writer_turn(ticket) {
                spin_loop();
                continue;
            }
            match self.inner_try_write_registered() {
                Some(guard) => {
                    // Keep the writer announced until WRITER is visible, so
                    // readers cannot enter through a zero-count window.
                    self.finish_writer_turn(ticket);
                    return guard;
                }
                None => spin_loop(),
            }
        }
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 获取WRITER守卫并关中断
    ///
    /// 等价于 Linux `__raw_write_lock_irqsave`：先关 IRQ 再关抢占，自旋期间两者始终关闭。
    pub fn write_irqsave(&self) -> RwLockWriteGuard<'_, T> {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::preempt_disable();
        let ticket = self
            .register_waiting_writer()
            .expect("too many pending RwLock writers");
        loop {
            if !self.writer_turn(ticket) {
                spin_loop();
                continue;
            }
            match self.inner_try_write_registered() {
                Some(mut guard) => {
                    self.finish_writer_turn(ticket);
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
    pub fn try_upgradeable_read(&self) -> Option<RwLockUpgradableGuard<'_, T>> {
        ProcessManager::preempt_disable();
        let r = self.inner_try_upgradeable_read();
        if r.is_none() {
            ProcessManager::preempt_enable();
        }

        return r;
    }

    #[allow(dead_code)]
    pub fn try_upgradeable_read_irqsave(&self) -> Option<RwLockUpgradableGuard<'_, T>> {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::preempt_disable();
        let mut r = self.inner_try_upgradeable_read();
        if let Some(r) = &mut r {
            r.irq_guard = Some(irq_guard);
        } else {
            ProcessManager::preempt_enable();
        }

        return r;
    }

    fn inner_try_upgradeable_read(&self) -> Option<RwLockUpgradableGuard<'_, T>> {
        // 获得UPGRADER守卫不需要查看读者位。使用CAS避免失败的尝试污染
        // WRITER状态中的UPGRADED位。
        let mut state = self.lock.load(Ordering::Relaxed);
        loop {
            if state & (WRITER | UPGRADED | PENDING_WRITER_MASK) != 0
                || state & READER_MASK == MAX_READERS * READER
            {
                return None;
            }
            state = match self.lock.compare_exchange_weak(
                state,
                state | UPGRADED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => actual,
            };
        }

        return Some(RwLockUpgradableGuard {
            inner: self,
            data: self.data.get().cast_const(),
            irq_guard: None,
        });
    }

    #[allow(dead_code)]
    #[inline]
    /// @brief 获得UPGRADER守卫
    pub fn upgradeable_read(&self) -> RwLockUpgradableGuard<'_, T> {
        loop {
            match self.try_upgradeable_read() {
                Some(guard) => return guard,
                None => spin_loop(),
            }
        }
    }

    #[inline]
    /// @brief 获得UPGRADER守卫并关中断
    ///
    /// 等价于 Linux irqsave 模式：先关 IRQ 再关抢占，自旋期间两者始终关闭。
    pub fn upgradeable_read_irqsave(&self) -> RwLockUpgradableGuard<'_, T> {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::preempt_disable();
        loop {
            match self.inner_try_upgradeable_read() {
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
        debug_assert!(self.lock.load(Ordering::Relaxed) & READER_MASK > 0);
        self.lock.fetch_sub(READER, Ordering::Release);
    }

    #[allow(dead_code)]
    #[inline]
    //extremely unsafe behavior
    /// @brief 强制给WRITER解锁
    pub unsafe fn force_write_unlock(&self) {
        debug_assert_eq!(self.lock.load(Ordering::Relaxed) & ACTIVE_MASK, WRITER);
        self.lock.fetch_and(!(WRITER | UPGRADED), Ordering::Release);
    }

    #[allow(dead_code)]
    pub unsafe fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.data.get() }
    }

    #[allow(dead_code)]
    pub unsafe fn force_get_ref(&self) -> &T {
        unsafe { &*self.data.get() }
    }

    /// `write_lock_bh()`：禁用本 CPU BH 后获取写锁。
    ///
    /// 注意：该接口不关硬中断；若该锁也会在 hardirq 获取，则必须使用 `write_irqsave()`。
    pub fn write_bh(&self) -> RwLockWriteBhGuard<'_, T> {
        let bh = local_bh_disable();
        let guard = self.write();
        RwLockWriteBhGuard { bh, guard }
    }
}

impl<T> Deref for RwLockReadBhGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.guard.deref()
    }
}

impl<T> Deref for RwLockWriteBhGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.guard.deref()
    }
}

impl<T> DerefMut for RwLockWriteBhGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.deref_mut()
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
        let mut state = self.inner.lock.load(Ordering::Relaxed);
        let res = loop {
            if state & ACTIVE_MASK != UPGRADED {
                break false;
            }
            let new_state = (state & PENDING_WRITER_MASK) | WRITER;
            state = match self.inner.lock.compare_exchange_weak(
                state,
                new_state,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break true,
                Err(actual) => actual,
            };
        };
        // 当且仅当只有UPGRADED所有者（可另有pending writer）时可以升级。

        if res {
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
        let inner: &RwLock<T> = self.inner;
        let irq_guard = self.irq_guard.take();

        // Convert the owned UPGRADED bit into a reader atomically.  This
        // conversion is allowed even when another writer is pending because
        // it does not admit a new lock owner.
        let mut state = inner.lock.load(Ordering::Relaxed);
        loop {
            debug_assert_eq!(state & (WRITER | UPGRADED), UPGRADED);
            assert_ne!(
                state & READER_MASK,
                MAX_READERS * READER,
                "RwLock reader count overflow during upgrader downgrade"
            );
            let new_state = (state - UPGRADED) + READER;
            state = match inner.lock.compare_exchange_weak(
                state,
                new_state,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => actual,
            };
        }

        // forget self 以跳过 UpgradableGuard::drop 中的 preempt_enable
        // 新的 ReadGuard 接管 preempt_count 所有权
        mem::forget(self);

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
        let inner = self.inner;
        let irq_guard = self.irq_guard.take();

        // No reader can coexist with WRITER, so this is a single atomic state
        // transition.  Pending writers remain announced in their own counter.
        let mut state = inner.lock.load(Ordering::Relaxed);
        loop {
            assert_eq!(state & ACTIVE_MASK, WRITER, "invalid RwLock writer state");
            let new_state = (state & PENDING_WRITER_MASK) | READER;
            state = match inner.lock.compare_exchange_weak(
                state,
                new_state,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => actual,
            };
        }

        // forget self 以跳过 WriteGuard::drop 中的 preempt_enable
        // 新的 ReadGuard 接管 preempt_count 所有权
        mem::forget(self);

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

        let mut state = self.inner.lock.load(Ordering::Relaxed);
        loop {
            assert_eq!(state & ACTIVE_MASK, WRITER, "invalid RwLock writer state");
            let new_state = (state & PENDING_WRITER_MASK) | UPGRADED;
            state = match self.inner.lock.compare_exchange_weak(
                state,
                new_state,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => actual,
            };
        }

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

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        return unsafe { &*self.data };
    }
}

impl<T> Deref for RwLockUpgradableGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        return unsafe { &*self.data };
    }
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        return unsafe { &*self.data };
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        return unsafe { &mut *self.data };
    }
}

impl<T> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        debug_assert!(self.lock.load(Ordering::Relaxed) & READER_MASK > 0);
        self.lock.fetch_sub(READER, Ordering::Release);
        // 先恢复中断，再启用抢占
        self.irq_guard.take();
        ProcessManager::preempt_enable();
    }
}

impl<T> Drop for RwLockUpgradableGuard<'_, T> {
    fn drop(&mut self) {
        debug_assert_eq!(
            self.inner.lock.load(Ordering::Relaxed) & (WRITER | UPGRADED),
            UPGRADED
        );
        self.inner.lock.fetch_and(!UPGRADED, Ordering::Release);
        // 先恢复中断，再启用抢占
        self.irq_guard.take();
        ProcessManager::preempt_enable();
    }
}

impl<T> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        debug_assert_eq!(self.inner.lock.load(Ordering::Relaxed) & WRITER, WRITER);
        self.inner
            .lock
            .fetch_and(!(WRITER | UPGRADED), Ordering::Release);
        // 先恢复中断，再启用抢占
        self.irq_guard.take();
        ProcessManager::preempt_enable();
    }
}

#[path = "rwlock_selftest.rs"]
mod selftest;

pub(crate) use selftest::run_rwlock_selftests;
