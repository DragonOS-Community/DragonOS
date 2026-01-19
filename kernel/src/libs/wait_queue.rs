// SPDX-License-Identifier: GPL-2.0-or-later
//
// Wait Queue implementation with wait_until as the core primitive.
//
// The wait_until family returns Option<R>, allowing direct return of acquired resources.
// The wait_event family (returning bool) is implemented on top of wait_until for compatibility.

use core::{
    intrinsics::unlikely,
    marker::PhantomData,
    mem,
    sync::atomic::{AtomicU32, AtomicU8, Ordering},
};

use alloc::{
    boxed::Box,
    collections::VecDeque,
    rc::Rc,
    sync::{Arc, Weak},
    vec::Vec,
};
use log::warn;
use system_error::SystemError;

use crate::{
    arch::{ipc::signal::Signal, CurrentIrqArch},
    exception::InterruptArch,
    libs::mutex::MutexGuard,
    process::{ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState},
    sched::{schedule, SchedMode},
    time::{
        timer::{next_n_us_timer_jiffies, Timer},
        Duration, Instant,
    },
};

use super::spinlock::{SpinLock, SpinLockGuard};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum WaitSignalMode {
    Uninterruptible,
    Interruptible,
    Killable,
}

#[derive(Debug)]
struct InnerWaitQueue {
    dead: bool,
    waiters: VecDeque<Arc<Waker>>,
}

/// 等待队列：基于一次性 Waiter/Waker，避免唤醒丢失
#[derive(Debug)]
pub struct WaitQueue {
    inner: SpinLock<InnerWaitQueue>,
    num_waiters: AtomicU32,
}

/// 属于当前线程的等待者，不可跨线程共享
pub struct Waiter {
    waker: Arc<Waker>,
    _nosend: PhantomData<Rc<()>>,
}

/// 可跨 CPU/线程共享的唤醒器
#[derive(Debug)]
pub struct Waker {
    state: AtomicU8,
    target: Weak<ProcessControlBlock>,
}

#[allow(dead_code)]
impl WaitQueue {
    pub const fn default() -> Self {
        WaitQueue {
            inner: SpinLock::new(InnerWaitQueue::INIT),
            num_waiters: AtomicU32::new(0),
        }
    }

    pub fn register_waker(&self, waker: Arc<Waker>) -> Result<(), SystemError> {
        let mut guard = self.inner.lock_irqsave();
        if guard.dead {
            return Err(SystemError::ECHILD);
        }
        guard.waiters.push_back(waker);
        self.num_waiters.fetch_add(1, Ordering::Release);
        Ok(())
    }

    pub fn remove_waker(&self, target: &Arc<Waker>) {
        let mut guard = self.inner.lock_irqsave();
        let before = guard.waiters.len();
        guard.waiters.retain(|w| !Arc::ptr_eq(w, target));
        let removed = before - guard.waiters.len();
        if removed > 0 {
            self.num_waiters
                .fetch_sub(removed as u32, Ordering::Release);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.num_waiters.fetch_add(0, Ordering::Acquire) == 0
    }

    // ==================== Core API: wait_until family ====================

    /// Waits until condition returns `Some(R)` (signals are ignored; use
    /// `wait_until_interruptible` for signal-aware waits).
    ///
    /// This is the core waiting primitive. The condition closure is called repeatedly
    /// until it returns `Some(_)`. The waker is registered BEFORE checking the condition
    /// to avoid missed wakeups.
    ///
    /// # Example
    /// ```ignore
    /// // Directly acquire a lock guard without race condition
    /// let guard = queue.wait_until(|| mutex.try_lock());
    /// ```
    #[track_caller]
    pub fn wait_until<F, R>(&self, cond: F) -> R
    where
        F: FnMut() -> Option<R>,
    {
        self.wait_until_impl(cond, WaitSignalMode::Uninterruptible, None, None::<fn()>)
            .unwrap()
    }

    /// Waits until condition returns `Some(R)` (interruptible by signals).
    ///
    /// Returns `Err(ERESTARTSYS)` if interrupted by a signal.
    #[track_caller]
    pub fn wait_until_interruptible<F, R>(&self, cond: F) -> Result<R, SystemError>
    where
        F: FnMut() -> Option<R>,
    {
        self.wait_until_impl(cond, WaitSignalMode::Interruptible, None, None::<fn()>)
    }

    #[track_caller]
    pub fn wait_until_killable<F, R>(&self, cond: F) -> Result<R, SystemError>
    where
        F: FnMut() -> Option<R>,
    {
        self.wait_until_impl(cond, WaitSignalMode::Killable, None, None::<fn()>)
    }

    /// Waits until condition returns `Some(R)` with timeout (interruptible).
    ///
    /// Returns:
    /// - `Ok(R)` if condition satisfied
    /// - `Err(ERESTARTSYS)` if interrupted by signal
    /// - `Err(EAGAIN_OR_EWOULDBLOCK)` if timeout
    #[track_caller]
    pub fn wait_until_timeout<F, R>(&self, cond: F, timeout: Duration) -> Result<R, SystemError>
    where
        F: FnMut() -> Option<R>,
    {
        self.wait_until_impl(
            cond,
            WaitSignalMode::Interruptible,
            Some(timeout),
            None::<fn()>,
        )
    }

    /// Core implementation for all wait_until variants.
    ///
    /// Key design:
    /// - Create only ONE waiter/waker pair
    /// - Enqueue the waker BEFORE each condition check
    /// - This ensures no wakeup is lost between check and sleep
    fn wait_until_impl<F, R, B>(
        &self,
        mut cond: F,
        mode: WaitSignalMode,
        timeout: Option<Duration>,
        mut before_sleep: Option<B>,
    ) -> Result<R, SystemError>
    where
        F: FnMut() -> Option<R>,
        B: FnMut(),
    {
        // Fast path: check condition first
        if let Some(res) = cond() {
            return Ok(res);
        }

        let deadline = timeout.map(|t| Instant::now() + t);

        // Create only ONE waiter/waker pair (key difference from old implementation)
        let (waiter, waker) = Waiter::new_pair();

        fn cancel_or_timeout<F, R>(
            cond: &mut F,
            waker: &Arc<Waker>,
            err: SystemError,
        ) -> Result<R, SystemError>
        where
            F: FnMut() -> Option<R>,
        {
            // Close to prevent a concurrent wake from being lost.
            waker.close();
            if let Some(res) = cond() {
                return Ok(res);
            }
            Err(err)
        }

        loop {
            // Check timeout
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    self.remove_waker(&waker);
                    return cancel_or_timeout(
                        &mut cond,
                        &waker,
                        SystemError::EAGAIN_OR_EWOULDBLOCK,
                    );
                }
            }

            // Enqueue waker BEFORE checking condition (critical for correctness!)
            // This ensures that if condition becomes true after our check,
            // the subsequent wake_one() will find our waker in the queue.
            if self.register_waker(waker.clone()).is_err() {
                // Queue is dead, spin until condition is met
                loop {
                    if let Some(res) = cond() {
                        return Ok(res);
                    }
                    core::hint::spin_loop();
                }
            }

            // Check condition AFTER enqueuing
            if let Some(res) = cond() {
                // Condition satisfied, remove waker and return
                self.remove_waker(&waker);
                return Ok(res);
            }

            let has_pending = match mode {
                WaitSignalMode::Uninterruptible => false,
                WaitSignalMode::Interruptible => {
                    Signal::signal_pending_state(true, false, &ProcessManager::current_pcb())
                }
                WaitSignalMode::Killable => {
                    Signal::signal_pending_state(false, true, &ProcessManager::current_pcb())
                }
            };
            if has_pending {
                self.remove_waker(&waker);
                return cancel_or_timeout(&mut cond, &waker, SystemError::ERESTARTSYS);
            }

            // Execute before_sleep hook (e.g., release a lock)
            if let Some(ref mut hook) = before_sleep {
                hook();
            }

            // Setup timeout timer if needed
            let timer = if let Some(deadline) = deadline {
                let remain = deadline
                    .duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO);
                if remain == Duration::ZERO {
                    self.remove_waker(&waker);
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                let sleep_us = remain.total_micros();
                let t: Arc<Timer> = Timer::new(
                    TimeoutWaker::new(waker.clone()),
                    next_n_us_timer_jiffies(sleep_us),
                );
                t.activate();
                Some(t)
            } else {
                None
            };

            // Wait to be woken
            let wait_result = match mode {
                WaitSignalMode::Uninterruptible => waiter.wait(false),
                WaitSignalMode::Interruptible => waiter.wait(true),
                WaitSignalMode::Killable => waiter.wait_killable(),
            };

            // Timer cleanup
            let was_timeout = timer.as_ref().map(|t| t.timeout()).unwrap_or(false);
            if !was_timeout {
                if let Some(t) = timer {
                    t.cancel();
                }
            }

            // Handle wait error (signal interruption)
            if let Err(e) = wait_result {
                self.remove_waker(&waker);
                return cancel_or_timeout(&mut cond, &waker, e);
            }

            // Note: We do NOT check condition here without re-enqueuing!
            // The loop will re-enqueue the waker and check condition again.
            // This is critical: if we checked condition here and it returned None,
            // and then someone called wake_one(), the wakeup would be lost because
            // our waker was already popped by the previous wake.
        }
    }

    // ==================== Compatibility API: wait_event family ====================
    // These are implemented on top of wait_until for backward compatibility.

    /// 可中断等待条件成立；`before_sleep` 在入队后、睡眠前执行（用于解锁等操作）
    pub fn wait_event_interruptible<F, B>(
        &self,
        mut cond: F,
        before_sleep: Option<B>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
        B: FnMut(),
    {
        self.wait_until_impl(
            || if cond() { Some(()) } else { None },
            WaitSignalMode::Interruptible,
            None,
            before_sleep,
        )
    }

    pub fn wait_event_killable<F, B>(
        &self,
        mut cond: F,
        before_sleep: Option<B>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
        B: FnMut(),
    {
        self.wait_until_impl(
            || if cond() { Some(()) } else { None },
            WaitSignalMode::Killable,
            None,
            before_sleep,
        )
    }

    /// 不可中断等待条件成立
    pub fn wait_event_uninterruptible<F, B>(
        &self,
        mut cond: F,
        before_sleep: Option<B>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
        B: FnMut(),
    {
        self.wait_until_impl(
            || if cond() { Some(()) } else { None },
            WaitSignalMode::Uninterruptible,
            None,
            before_sleep,
        )
    }

    /// 可中断等待条件成立，支持可选超时。
    ///
    /// - `timeout == None`：无限等待，直到条件成立或收到信号（返回 `ERESTARTSYS`）
    /// - `timeout == Some(d)`：超时返回 `EAGAIN_OR_EWOULDBLOCK`
    pub fn wait_event_interruptible_timeout<F>(
        &self,
        mut cond: F,
        timeout: Option<Duration>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
    {
        self.wait_until_impl(
            || if cond() { Some(()) } else { None },
            WaitSignalMode::Interruptible,
            timeout,
            None::<fn()>,
        )
    }

    /// 不可中断等待条件成立，支持可选超时。
    ///
    /// - `timeout == None`：无限等待，直到条件成立。
    /// - `timeout == Some(d)`：超时返回 `EAGAIN_OR_EWOULDBLOCK`。
    pub fn wait_event_uninterruptible_timeout<F>(
        &self,
        mut cond: F,
        timeout: Option<Duration>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
    {
        self.wait_until_impl(
            || if cond() { Some(()) } else { None },
            WaitSignalMode::Uninterruptible,
            timeout,
            None::<fn()>,
        )
    }

    /// `wait_event_interruptible_timeout` 的扩展版本：提供 `before_sleep` 钩子。
    ///
    /// 典型用途：入队后、睡眠前释放锁（避免持锁睡眠）。
    pub fn wait_event_interruptible_timeout_with<F, B>(
        &self,
        mut cond: F,
        timeout: Option<Duration>,
        before_sleep: Option<B>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
        B: FnMut(),
    {
        self.wait_until_impl(
            || if cond() { Some(()) } else { None },
            WaitSignalMode::Interruptible,
            timeout,
            before_sleep,
        )
    }

    // ==================== Wakeup API ====================

    /// 唤醒一个等待者
    pub fn wakeup(&self, _state: Option<ProcessState>) -> bool {
        self.wake_one()
    }

    pub fn wake_one(&self) -> bool {
        if self.is_empty() {
            return false;
        }

        loop {
            let next = {
                let mut guard = self.inner.lock_irqsave();
                let waker = guard.waiters.pop_front();
                if waker.is_some() {
                    self.num_waiters.fetch_sub(1, Ordering::Release);
                }
                waker
            };

            let Some(waker) = next else { return false };
            if waker.wake() {
                return true;
            }
        }
    }

    /// 唤醒队列中全部等待者
    pub fn wakeup_all(&self, _state: Option<ProcessState>) {
        self.wake_all();
    }

    pub fn wake_all(&self) -> usize {
        if self.is_empty() {
            return 0;
        }

        let mut drained = VecDeque::new();
        {
            let mut guard = self.inner.lock_irqsave();
            mem::swap(&mut guard.waiters, &mut drained);
            self.num_waiters.store(0, Ordering::Release);
        }

        let wakers = drained;
        let mut woken = 0;
        for w in wakers {
            if w.wake() {
                woken += 1;
            }
        }
        woken
    }

    /// 标记等待队列失效，清空并唤醒剩余等待者
    pub fn mark_dead(&self) {
        let mut drained = VecDeque::new();
        {
            let mut guard = self.inner.lock_irqsave();
            guard.dead = true;
            mem::swap(&mut guard.waiters, &mut drained);
            self.num_waiters.store(0, Ordering::Release);
        }
        for w in drained {
            w.wake();
            w.close();
        }
    }

    pub fn len(&self) -> usize {
        self.num_waiters.fetch_add(0, Ordering::Acquire) as usize
    }

    // ==================== Sleep with lock release ====================

    pub fn sleep_unlock_spinlock<T>(&self, to_unlock: SpinLockGuard<T>) -> Result<(), SystemError> {
        before_sleep_check(1);
        let mut to_unlock = Some(to_unlock);
        self.wait_event_interruptible(
            || false,
            Some(move || {
                if let Some(lock) = to_unlock.take() {
                    drop(lock);
                }
            }),
        )
    }

    pub fn sleep_unlock_mutex<T>(&self, to_unlock: MutexGuard<T>) -> Result<(), SystemError> {
        before_sleep_check(1);
        let mut to_unlock = Some(to_unlock);
        self.wait_event_interruptible(
            || false,
            Some(move || {
                if let Some(lock) = to_unlock.take() {
                    drop(lock);
                }
            }),
        )
    }

    pub fn sleep_uninterruptible_unlock_spinlock<T>(&self, to_unlock: SpinLockGuard<T>) {
        before_sleep_check(1);
        let mut to_unlock = Some(to_unlock);
        let _ = self.wait_event_uninterruptible(
            || false,
            Some(move || {
                if let Some(lock) = to_unlock.take() {
                    drop(lock);
                }
            }),
        );
    }

    pub fn sleep_uninterruptible_unlock_mutex<T>(&self, to_unlock: MutexGuard<T>) {
        before_sleep_check(1);
        let mut to_unlock = Some(to_unlock);
        let _ = self.wait_event_uninterruptible(
            || false,
            Some(move || {
                if let Some(lock) = to_unlock.take() {
                    drop(lock);
                }
            }),
        );
    }
}

impl InnerWaitQueue {
    pub const INIT: InnerWaitQueue = InnerWaitQueue {
        dead: false,
        waiters: VecDeque::new(),
    };
}

impl Waiter {
    pub fn new_pair() -> (Self, Arc<Waker>) {
        let waker = Arc::new(Waker {
            state: AtomicU8::new(Waker::STATE_IDLE),
            target: Arc::downgrade(&ProcessManager::current_pcb()),
        });
        let waiter = Waiter {
            waker: waker.clone(),
            _nosend: PhantomData,
        };
        (waiter, waker)
    }

    pub fn wait(&self, interruptible: bool) -> Result<(), SystemError> {
        block_current(self, interruptible)
    }

    pub fn wait_killable(&self) -> Result<(), SystemError> {
        block_current_killable(self)
    }

    pub fn waker(&self) -> Arc<Waker> {
        self.waker.clone()
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        self.waker.close();
    }
}

impl Waker {
    const STATE_IDLE: u8 = 0;
    const STATE_SLEEPING: u8 = 1;
    const STATE_NOTIFIED: u8 = 2;
    const STATE_CLOSED: u8 = 3;

    pub fn wake(&self) -> bool {
        loop {
            let state = self.state.load(Ordering::Acquire);
            match state {
                Self::STATE_CLOSED | Self::STATE_NOTIFIED => return false,
                Self::STATE_IDLE => {
                    if self
                        .state
                        .compare_exchange(
                            Self::STATE_IDLE,
                            Self::STATE_NOTIFIED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
                Self::STATE_SLEEPING => {
                    if self
                        .state
                        .compare_exchange(
                            Self::STATE_SLEEPING,
                            Self::STATE_NOTIFIED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        if let Some(pcb) = self.target.upgrade() {
                            let _ = ProcessManager::wakeup(&pcb);
                        }
                        return true;
                    }
                }
                _ => core::hint::spin_loop(),
            }
        }
    }

    #[inline]
    pub fn close(&self) {
        self.state.store(Self::STATE_CLOSED, Ordering::Release);
    }

    fn consume_notification(&self) -> bool {
        loop {
            let state = self.state.load(Ordering::Acquire);
            match state {
                Self::STATE_NOTIFIED => {
                    if self
                        .state
                        .compare_exchange(
                            Self::STATE_NOTIFIED,
                            Self::STATE_IDLE,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
                Self::STATE_CLOSED => return true,
                _ => return false,
            }
        }
    }

    fn prepare_sleep(&self) -> WakerSleepState {
        loop {
            let state = self.state.load(Ordering::Acquire);
            match state {
                Self::STATE_IDLE => {
                    if self
                        .state
                        .compare_exchange(
                            Self::STATE_IDLE,
                            Self::STATE_SLEEPING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return WakerSleepState::Sleeping;
                    }
                }
                Self::STATE_SLEEPING => return WakerSleepState::Sleeping,
                Self::STATE_NOTIFIED => return WakerSleepState::Notified,
                Self::STATE_CLOSED => return WakerSleepState::Closed,
                _ => core::hint::spin_loop(),
            }
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum WakerSleepState {
    Sleeping,
    Notified,
    Closed,
}

fn before_sleep_check(max_preempt: usize) {
    let pcb = ProcessManager::current_pcb();
    if unlikely(pcb.preempt_count() > max_preempt) {
        warn!(
            "Process {:?}: Try to sleep when preempt count is {}",
            pcb.raw_pid().data(),
            pcb.preempt_count()
        );
    }
}

/// 统一封装"标记阻塞 + 调度 + 信号检查"，避免各调用点重复逻辑
fn block_current(waiter: &Waiter, interruptible: bool) -> Result<(), SystemError> {
    loop {
        // 快路径：被提前唤醒
        if waiter.waker.consume_notification() {
            return Ok(());
        }

        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        // Build a waiter/waker handshake to avoid lost wakeups.
        match waiter.waker.prepare_sleep() {
            WakerSleepState::Notified | WakerSleepState::Closed => {
                drop(irq_guard);
                return Ok(());
            }
            WakerSleepState::Sleeping => {}
        }

        ProcessManager::mark_sleep(interruptible)?;

        // Handle a wake racing between prepare_sleep and mark_sleep.
        if waiter.waker.consume_notification() {
            let pcb = ProcessManager::current_pcb();
            let mut writer = pcb.sched_info().inner_lock_write_irqsave();
            if matches!(writer.state(), ProcessState::Blocked(_)) {
                writer.set_state(ProcessState::Runnable);
                writer.set_wakeup();
            }
            pcb.flags().remove(ProcessFlags::NEED_SCHEDULE);
            drop(writer);
            drop(irq_guard);
            return Ok(());
        }
        drop(irq_guard);

        schedule(SchedMode::SM_NONE);

        if interruptible
            && Signal::signal_pending_state(true, false, &ProcessManager::current_pcb())
        {
            return Err(SystemError::ERESTARTSYS);
        }
    }
}

fn block_current_killable(waiter: &Waiter) -> Result<(), SystemError> {
    loop {
        if waiter.waker.consume_notification() {
            return Ok(());
        }

        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        match waiter.waker.prepare_sleep() {
            WakerSleepState::Notified | WakerSleepState::Closed => {
                drop(irq_guard);
                return Ok(());
            }
            WakerSleepState::Sleeping => {}
        }

        ProcessManager::mark_sleep(true)?;

        if waiter.waker.consume_notification() {
            let pcb = ProcessManager::current_pcb();
            let mut writer = pcb.sched_info().inner_lock_write_irqsave();
            if matches!(writer.state(), ProcessState::Blocked(_)) {
                writer.set_state(ProcessState::Runnable);
                writer.set_wakeup();
            }
            pcb.flags().remove(ProcessFlags::NEED_SCHEDULE);
            drop(writer);
            drop(irq_guard);
            return Ok(());
        }
        drop(irq_guard);

        schedule(SchedMode::SM_NONE);

        if Signal::signal_pending_state(false, true, &ProcessManager::current_pcb()) {
            return Err(SystemError::ERESTARTSYS);
        }
    }
}

/// 事件等待队列：按事件掩码唤醒
#[derive(Debug)]
pub struct EventWaitQueue {
    wait_list: SpinLock<Vec<(u64, Arc<Waker>)>>,
}

impl Default for EventWaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
impl EventWaitQueue {
    pub fn new() -> Self {
        Self {
            wait_list: SpinLock::new(Default::default()),
        }
    }

    pub fn sleep(&self, events: u64) {
        before_sleep_check(0);
        let (waiter, waker) = Waiter::new_pair();
        {
            let mut guard = self.wait_list.lock_irqsave();
            guard.push((events, waker));
        }
        let _ = waiter.wait(true);
    }

    pub fn sleep_unlock_spinlock<T>(&self, events: u64, to_unlock: SpinLockGuard<T>) {
        before_sleep_check(1);
        let (waiter, waker) = Waiter::new_pair();
        {
            let mut guard = self.wait_list.lock_irqsave();
            guard.push((events, waker));
        }
        drop(to_unlock);
        let _ = waiter.wait(true);
    }

    pub fn wakeup_any(&self, events: u64) -> usize {
        let mut ret = 0;
        let mut guard = self.wait_list.lock_irqsave();
        guard.retain(|(es, waker)| {
            if *es & events > 0 {
                if waker.wake() {
                    ret += 1;
                }
                false
            } else {
                true
            }
        });
        ret
    }

    pub fn wakeup(&self, events: u64) -> usize {
        let mut ret = 0;
        let mut guard = self.wait_list.lock_irqsave();
        guard.retain(|(es, waker)| {
            if *es == events {
                if waker.wake() {
                    ret += 1;
                }
                false
            } else {
                true
            }
        });
        ret
    }

    pub fn wakeup_all(&self) {
        self.wakeup_any(u64::MAX);
    }
}

/// 通用的超时唤醒辅助结构
///
/// 用于定时器超时时唤醒等待队列中的 Waiter。
/// 相比直接唤醒 PCB，通过 Waker 唤醒可以：
/// 1. 与 Waiter::wait() 正确配合，避免竞态条件
/// 2. 使用原子状态机标记唤醒/睡眠状态
/// 3. 保持与等待队列机制的一致性
#[derive(Debug)]
pub struct TimeoutWaker {
    waker: Arc<Waker>,
}

impl TimeoutWaker {
    pub fn new(waker: Arc<Waker>) -> Box<Self> {
        Box::new(Self { waker })
    }
}

impl crate::time::timer::TimerFunction for TimeoutWaker {
    fn run(&mut self) -> Result<(), SystemError> {
        // 通过 Waker::wake() 唤醒，这样 Waiter::wait() 可以观察到
        // 注意：定时器唤醒必须通过 Waker::wake()，仅唤醒 PCB 是不够的
        self.waker.wake();
        Ok(())
    }
}
