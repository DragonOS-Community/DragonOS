// #![allow(dead_code)]
use core::{
    intrinsics::unlikely,
    marker::PhantomData,
    mem,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
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
    process::{ProcessControlBlock, ProcessManager, ProcessState},
    sched::{schedule, SchedMode},
};

use super::{
    mutex::MutexGuard,
    spinlock::{SpinLock, SpinLockGuard},
};

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
    has_woken: AtomicBool,
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

    /// 可中断等待条件成立；`before_sleep` 在入队后、睡眠前执行（用于解锁等操作）
    pub fn wait_event_interruptible<F, B>(
        &self,
        cond: F,
        before_sleep: Option<B>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
        B: FnMut(),
    {
        self.wait_event_impl(cond, before_sleep, true)
    }

    /// 不可中断等待条件成立
    pub fn wait_event_uninterruptible<F, B>(
        &self,
        cond: F,
        before_sleep: Option<B>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
        B: FnMut(),
    {
        self.wait_event_impl(cond, before_sleep, false)
    }

    fn wait_event_impl<F, B>(
        &self,
        mut cond: F,
        mut before_sleep: Option<B>,
        interruptible: bool,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
        B: FnMut(),
    {
        loop {
            if cond() {
                return Ok(());
            }

            let (waiter, waker) = Waiter::new_pair();
            self.register_waker(waker.clone())?;

            // 条件可能在入队后立即满足，直接摘掉
            if cond() {
                self.remove_waker(&waker);
                return Ok(());
            }

            if interruptible
                && Signal::signal_pending_state(true, false, &ProcessManager::current_pcb())
            {
                self.remove_waker(&waker);
                return Err(SystemError::ERESTARTSYS);
            }

            if let Some(ref mut hook) = before_sleep {
                hook();
            }

            match waiter.wait(interruptible) {
                Ok(()) => {
                    // 再次循环检查条件，处理伪唤醒
                    continue;
                }
                Err(e) => {
                    self.remove_waker(&waker);
                    return Err(e);
                }
            }
        }
    }

    /// 唤醒一个等待者
    pub fn wakeup(&self, _state: Option<ProcessState>) -> bool {
        // state 参数保留兼容位置（现阶段未再使用）
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
            w.close();
            w.wake();
        }
    }

    pub fn len(&self) -> usize {
        self.num_waiters.fetch_add(0, Ordering::Acquire) as usize
    }

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
            has_woken: AtomicBool::new(false),
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
    #[inline]
    pub fn wake(&self) -> bool {
        if self.has_woken.swap(true, Ordering::Release) {
            return false;
        }
        if let Some(pcb) = self.target.upgrade() {
            let _ = ProcessManager::wakeup(&pcb);
        }
        true
    }

    #[inline]
    pub fn close(&self) {
        let _ = self.has_woken.swap(true, Ordering::Acquire);
    }

    #[inline]
    fn consume_wake(&self) -> bool {
        self.has_woken.swap(false, Ordering::Acquire)
    }
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

/// 统一封装“标记阻塞 + 调度 + 信号检查”，避免各调用点重复逻辑
fn block_current(waiter: &Waiter, interruptible: bool) -> Result<(), SystemError> {
    loop {
        // 快路径：被提前唤醒
        if waiter.waker.consume_wake() {
            return Ok(());
        }

        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        // 再次检查“先唤后睡”窗口
        if waiter.waker.consume_wake() {
            drop(irq_guard);
            return Ok(());
        }

        ProcessManager::mark_sleep(interruptible)?;
        drop(irq_guard);

        schedule(SchedMode::SM_NONE);

        if interruptible
            && Signal::signal_pending_state(true, false, &ProcessManager::current_pcb())
        {
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
/// 2. 使用原子标志 has_woken 标记唤醒状态
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
