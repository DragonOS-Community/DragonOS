// #![allow(dead_code)]
use core::intrinsics::unlikely;

use alloc::{collections::LinkedList, sync::Arc, vec::Vec};

use crate::{
    arch::{sched::sched, CurrentIrqArch},
    exception::InterruptArch,
    kerror,
    process::{ProcessControlBlock, ProcessManager, ProcessState},
};

use super::{
    mutex::MutexGuard,
    spinlock::{SpinLock, SpinLockGuard},
};

#[derive(Debug)]
struct InnerWaitQueue {
    /// 等待队列的链表
    wait_list: LinkedList<Arc<ProcessControlBlock>>,
}

/// 被自旋锁保护的等待队列
#[derive(Debug)]
pub struct WaitQueue(SpinLock<InnerWaitQueue>);

#[allow(dead_code)]
impl WaitQueue {
    pub const INIT: WaitQueue = WaitQueue(SpinLock::new(InnerWaitQueue::INIT));

    /// @brief 让当前进程在等待队列上进行等待，并且，允许被信号打断
    pub fn sleep(&self) {
        before_sleep_check(0);
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock_irqsave();
        ProcessManager::mark_sleep(true).unwrap_or_else(|e| {
            panic!("sleep error: {:?}", e);
        });
        guard.wait_list.push_back(ProcessManager::current_pcb());
        drop(guard);
        sched();
    }

    /// @brief 让当前进程在等待队列上进行等待，并且,在释放waitqueue的锁之前，执行f函数闭包
    pub fn sleep_with_func<F>(&self, f: F)
    where
        F: FnOnce(),
    {
        before_sleep_check(0);
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(true).unwrap_or_else(|e| {
            panic!("sleep error: {:?}", e);
        });
        drop(irq_guard);
        guard.wait_list.push_back(ProcessManager::current_pcb());
        f();

        drop(guard);
        sched();
    }

    /// @brief 让当前进程在等待队列上进行等待. 但是，在释放waitqueue的锁之后，不会调用调度函数。
    /// 这样的设计，是为了让调用者可以在执行本函数之后，执行一些操作，然后再【手动调用调度函数】。
    ///
    /// 执行本函数前，需要确保处于【中断禁止】状态。
    ///
    /// 尽管sleep_with_func和sleep_without_schedule都可以实现这个功能，但是，sleep_with_func会在释放锁之前，执行f函数闭包。
    ///
    /// 考虑这样一个场景：
    /// 等待队列位于某个自旋锁保护的数据结构A中，我们希望在进程睡眠的同时，释放数据结构A的锁。
    /// 在这种情况下，如果使用sleep_with_func，所有权系统不会允许我们这么做。
    /// 因此，sleep_without_schedule的设计，正是为了解决这个问题。
    ///
    /// 由于sleep_without_schedule不会调用调度函数，因此，如果开发者忘记在执行本函数之后，手动调用调度函数，
    /// 由于时钟中断到来或者‘其他cpu kick了当前cpu’，可能会导致一些未定义的行为。
    pub unsafe fn sleep_without_schedule(&self) {
        before_sleep_check(1);
        // 安全检查：确保当前处于中断禁止状态
        assert!(CurrentIrqArch::is_irq_enabled() == false);
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        ProcessManager::mark_sleep(true).unwrap_or_else(|e| {
            panic!("sleep error: {:?}", e);
        });
        guard.wait_list.push_back(ProcessManager::current_pcb());
        drop(guard);
    }

    pub unsafe fn sleep_without_schedule_uninterruptible(&self) {
        before_sleep_check(0);
        // 安全检查：确保当前处于中断禁止状态
        assert!(CurrentIrqArch::is_irq_enabled() == false);
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        ProcessManager::mark_sleep(false).unwrap_or_else(|e| {
            panic!("sleep error: {:?}", e);
        });
        guard.wait_list.push_back(ProcessManager::current_pcb());
        drop(guard);
    }
    /// @brief 让当前进程在等待队列上进行等待，并且，不允许被信号打断
    pub fn sleep_uninterruptible(&self) {
        before_sleep_check(0);
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(false).unwrap_or_else(|e| {
            panic!("sleep error: {:?}", e);
        });
        drop(irq_guard);
        guard.wait_list.push_back(ProcessManager::current_pcb());
        drop(guard);
        sched();
    }

    /// @brief 让当前进程在等待队列上进行等待，并且，允许被信号打断。
    /// 在当前进程的pcb加入队列后，解锁指定的自旋锁。
    pub fn sleep_unlock_spinlock<T>(&self, to_unlock: SpinLockGuard<T>) {
        before_sleep_check(1);
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(true).unwrap_or_else(|e| {
            panic!("sleep error: {:?}", e);
        });
        drop(irq_guard);
        guard.wait_list.push_back(ProcessManager::current_pcb());
        drop(to_unlock);
        drop(guard);
        sched();
    }

    /// @brief 让当前进程在等待队列上进行等待，并且，允许被信号打断。
    /// 在当前进程的pcb加入队列后，解锁指定的Mutex。
    pub fn sleep_unlock_mutex<T>(&self, to_unlock: MutexGuard<T>) {
        before_sleep_check(1);
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(true).unwrap_or_else(|e| {
            panic!("sleep error: {:?}", e);
        });
        drop(irq_guard);
        guard.wait_list.push_back(ProcessManager::current_pcb());
        drop(to_unlock);
        drop(guard);
        sched();
    }

    /// @brief 让当前进程在等待队列上进行等待，并且，不允许被信号打断。
    /// 在当前进程的pcb加入队列后，解锁指定的自旋锁。
    pub fn sleep_uninterruptible_unlock_spinlock<T>(&self, to_unlock: SpinLockGuard<T>) {
        before_sleep_check(1);
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(false).unwrap_or_else(|e| {
            panic!("sleep error: {:?}", e);
        });
        drop(irq_guard);
        guard.wait_list.push_back(ProcessManager::current_pcb());
        drop(to_unlock);
        drop(guard);
        sched();
    }

    /// @brief 让当前进程在等待队列上进行等待，并且，不允许被信号打断。
    /// 在当前进程的pcb加入队列后，解锁指定的Mutex。
    pub fn sleep_uninterruptible_unlock_mutex<T>(&self, to_unlock: MutexGuard<T>) {
        before_sleep_check(1);
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(false).unwrap_or_else(|e| {
            panic!("sleep error: {:?}", e);
        });
        drop(irq_guard);

        guard.wait_list.push_back(ProcessManager::current_pcb());

        drop(to_unlock);
        drop(guard);
        sched();
    }

    /// @brief 唤醒在队列中等待的第一个进程。
    /// 如果这个进程的state与给定的state进行and操作之后，结果不为0,则唤醒它。
    ///
    /// @param state 用于判断的state，如果队列第一个进程与这个state相同，或者为None(表示不进行这个判断)，则唤醒这个进程。
    ///
    /// @return true 成功唤醒进程
    /// @return false 没有唤醒进程
    pub fn wakeup(&self, state: Option<ProcessState>) -> bool {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock_irqsave();
        // 如果队列为空，则返回
        if guard.wait_list.is_empty() {
            return false;
        }
        // 如果队列头部的pcb的state与给定的state相与，结果不为0，则唤醒
        if let Some(state) = state {
            if guard
                .wait_list
                .front()
                .unwrap()
                .sched_info()
                .inner_lock_read_irqsave()
                .state()
                != state
            {
                return false;
            }
        }
        let to_wakeup = guard.wait_list.pop_front().unwrap();
        drop(guard);
        let res = ProcessManager::wakeup(&to_wakeup).is_ok();
        return res;
    }

    /// @brief 唤醒在队列中，符合条件的所有进程。
    ///
    /// @param state 用于判断的state，如果一个进程与这个state相同，或者为None(表示不进行这个判断)，则唤醒这个进程。
    pub fn wakeup_all(&self, state: Option<ProcessState>) {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock_irqsave();
        // 如果队列为空，则返回
        if guard.wait_list.is_empty() {
            return;
        }

        let mut to_push_back: Vec<Arc<ProcessControlBlock>> = Vec::new();
        // 如果队列头部的pcb的state与给定的state相与，结果不为0，则唤醒
        while let Some(to_wakeup) = guard.wait_list.pop_front() {
            let mut wake = false;
            if let Some(state) = state {
                if to_wakeup.sched_info().inner_lock_read_irqsave().state() == state {
                    wake = true;
                }
            } else {
                wake = true;
            }

            if wake {
                ProcessManager::wakeup(&to_wakeup).unwrap_or_else(|e| {
                    kerror!("wakeup pid: {:?} error: {:?}", to_wakeup.pid(), e);
                });
                continue;
            } else {
                to_push_back.push(to_wakeup);
            }
        }

        for to_wakeup in to_push_back {
            guard.wait_list.push_back(to_wakeup);
        }
    }

    /// @brief 获得当前等待队列的大小
    pub fn len(&self) -> usize {
        return self.0.lock().wait_list.len();
    }
}

impl InnerWaitQueue {
    pub const INIT: InnerWaitQueue = InnerWaitQueue {
        wait_list: LinkedList::new(),
    };
}

fn before_sleep_check(max_preempt: usize) {
    let pcb = ProcessManager::current_pcb();
    if unlikely(pcb.preempt_count() > max_preempt) {
        kwarn!(
            "Process {:?}: Try to sleep when preempt count is {}",
            pcb.pid(),
            pcb.preempt_count()
        );
    }
}

/// 事件等待队列
#[derive(Debug)]
pub struct EventWaitQueue {
    wait_list: SpinLock<Vec<(u64, Arc<ProcessControlBlock>)>>,
}

#[allow(dead_code)]
impl EventWaitQueue {
    pub fn new() -> Self {
        Self {
            wait_list: SpinLock::new(Vec::new()),
        }
    }

    /// ## 让当前进程在该队列上等待感兴趣的事件
    ///
    /// ### 参数
    /// - events: 进程感兴趣的事件，events最好是为位表示，一位表示一个事件
    ///
    /// 注意，使用前应该注意有可能其他地方定义了冲突的事件，可能会导致未定义行为
    pub fn sleep(&self, events: u64) {
        before_sleep_check(0);
        let mut guard = self.wait_list.lock_irqsave();
        ProcessManager::mark_sleep(true).unwrap_or_else(|e| {
            panic!("sleep error: {:?}", e);
        });
        guard.push((events, ProcessManager::current_pcb()));
        drop(guard);
        sched();
    }

    pub unsafe fn sleep_without_schedule(&self, events: u64) {
        before_sleep_check(1);
        let mut guard = self.wait_list.lock_irqsave();
        ProcessManager::mark_sleep(true).unwrap_or_else(|e| {
            panic!("sleep error: {:?}", e);
        });
        guard.push((events, ProcessManager::current_pcb()));
        drop(guard);
    }

    pub fn sleep_unlock_spinlock<T>(&self, events: u64, to_unlock: SpinLockGuard<T>) {
        before_sleep_check(1);
        let mut guard = self.wait_list.lock_irqsave();
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(true).unwrap_or_else(|e| {
            panic!("sleep error: {:?}", e);
        });
        drop(irq_guard);
        guard.push((events, ProcessManager::current_pcb()));
        drop(to_unlock);
        drop(guard);
        sched();
    }

    /// ### 唤醒该队列上等待events的进程
    ///
    ///  ### 参数
    /// - events: 发生的事件
    ///
    /// 需要注意的是，只要触发了events中的任意一件事件，进程都会被唤醒
    pub fn wakeup_any(&self, events: u64) -> usize {
        let mut ret = 0;

        let mut wq_guard = self.wait_list.lock_irqsave();
        wq_guard.retain(|(es, pcb)| {
            if *es & events > 0 {
                // 有感兴趣的事件
                if ProcessManager::wakeup(pcb).is_ok() {
                    ret += 1;
                    return false;
                } else {
                    return true;
                }
            } else {
                return true;
            }
        });
        ret
    }

    /// ### 唤醒该队列上等待events的进程
    ///
    ///  ### 参数
    /// - events: 发生的事件
    ///
    /// 需要注意的是，只有满足所有事件的进程才会被唤醒
    pub fn wakeup(&self, events: u64) -> usize {
        let mut ret = 0;
        let mut wq_guard = self.wait_list.lock_irqsave();
        wq_guard.retain(|(es, pcb)| {
            if *es == events {
                // 有感兴趣的事件
                if ProcessManager::wakeup(pcb).is_ok() {
                    ret += 1;
                    return false;
                } else {
                    return true;
                }
            } else {
                return true;
            }
        });
        ret
    }

    pub fn wakeup_all(&self) {
        self.wakeup_any(u64::MAX);
    }
}
