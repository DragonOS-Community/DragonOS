#![allow(dead_code)]
use alloc::{collections::LinkedList, vec::Vec};

use crate::{
    arch::{asm::current::current_pcb, sched::sched, CurrentIrqArch},
    exception::InterruptArch,
    include::bindings::bindings::{
        process_control_block, process_wakeup, PROC_INTERRUPTIBLE, PROC_UNINTERRUPTIBLE,
    },
};

use super::{
    mutex::MutexGuard,
    spinlock::{SpinLock, SpinLockGuard},
};

#[derive(Debug)]
struct InnerWaitQueue {
    /// 等待队列的链表
    wait_list: LinkedList<&'static mut process_control_block>,
}

/// 被自旋锁保护的等待队列
#[derive(Debug)]
pub struct WaitQueue(SpinLock<InnerWaitQueue>);

impl WaitQueue {
    pub const INIT: WaitQueue = WaitQueue(SpinLock::new(InnerWaitQueue::INIT));

    /// @brief 让当前进程在等待队列上进行等待，并且，允许被信号打断
    pub fn sleep(&self) {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        current_pcb().state = PROC_INTERRUPTIBLE as u64;
        guard.wait_list.push_back(current_pcb());
        drop(guard);

        sched();
    }

    /// @brief 让当前进程在等待队列上进行等待，并且,在释放waitqueue的锁之前，执行f函数闭包
    pub fn sleep_with_func<F>(&self, f: F)
    where
        F: FnOnce(),
    {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        current_pcb().state = PROC_INTERRUPTIBLE as u64;
        guard.wait_list.push_back(current_pcb());
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
        // 安全检查：确保当前处于中断禁止状态
        assert!(CurrentIrqArch::is_irq_enabled() == false);
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        current_pcb().state = PROC_INTERRUPTIBLE as u64;
        guard.wait_list.push_back(current_pcb());
        drop(guard);
    }
    /// @brief 让当前进程在等待队列上进行等待，并且，不允许被信号打断
    pub fn sleep_uninterruptible(&self) {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        current_pcb().state = PROC_UNINTERRUPTIBLE as u64;
        guard.wait_list.push_back(current_pcb());
        drop(guard);
        sched();
    }

    /// @brief 让当前进程在等待队列上进行等待，并且，允许被信号打断。
    /// 在当前进程的pcb加入队列后，解锁指定的自旋锁。
    pub fn sleep_unlock_spinlock<T>(&self, to_unlock: SpinLockGuard<T>) {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        current_pcb().state = PROC_INTERRUPTIBLE as u64;
        guard.wait_list.push_back(current_pcb());
        drop(to_unlock);
        drop(guard);
        sched();
    }

    /// @brief 让当前进程在等待队列上进行等待，并且，允许被信号打断。
    /// 在当前进程的pcb加入队列后，解锁指定的Mutex。
    pub fn sleep_unlock_mutex<T>(&self, to_unlock: MutexGuard<T>) {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        current_pcb().state = PROC_INTERRUPTIBLE as u64;
        guard.wait_list.push_back(current_pcb());
        drop(to_unlock);
        drop(guard);
        sched();
    }

    /// @brief 让当前进程在等待队列上进行等待，并且，不允许被信号打断。
    /// 在当前进程的pcb加入队列后，解锁指定的自旋锁。
    pub fn sleep_uninterruptible_unlock_spinlock<T>(&self, to_unlock: SpinLockGuard<T>) {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        current_pcb().state = PROC_UNINTERRUPTIBLE as u64;
        guard.wait_list.push_back(current_pcb());
        drop(to_unlock);
        drop(guard);
        sched();
    }

    /// @brief 让当前进程在等待队列上进行等待，并且，不允许被信号打断。
    /// 在当前进程的pcb加入队列后，解锁指定的Mutex。
    pub fn sleep_uninterruptible_unlock_mutex<T>(&self, to_unlock: MutexGuard<T>) {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        current_pcb().state = PROC_UNINTERRUPTIBLE as u64;
        guard.wait_list.push_back(current_pcb());
        drop(to_unlock);
        drop(guard);
        sched();
    }

    /// @brief 唤醒在队列中等待的第一个进程。
    /// 如果这个进程的state与给定的state进行and操作之后，结果不为0,则唤醒它。
    ///
    /// @param state 用于判断的state，如果队列中第一个进程的state与它进行and操作之后，结果不为0,则唤醒这个进程。
    ///
    /// @return true 成功唤醒进程
    /// @return false 没有唤醒进程
    pub fn wakeup(&self, state: u64) -> bool {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        // 如果队列为空，则返回
        if guard.wait_list.is_empty() {
            return false;
        }

        // 如果队列头部的pcb的state与给定的state相与，结果不为0，则唤醒
        if (guard.wait_list.front().unwrap().state & state) != 0 {
            let to_wakeup = guard.wait_list.pop_front().unwrap();
            unsafe {
                process_wakeup(to_wakeup);
            }
            return true;
        } else {
            return false;
        }
    }

    /// @brief 唤醒在队列中，符合条件的所有进程。
    ///
    /// @param state 用于判断的state，如果队列中第一个进程的state与它进行and操作之后，结果不为0,则唤醒这个进程。
    pub fn wakeup_all(&self, state: u64) {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock_irqsave();
        // 如果队列为空，则返回
        if guard.wait_list.is_empty() {
            return;
        }

        let mut to_push_back: Vec<&mut process_control_block> = Vec::new();
        // 如果队列头部的pcb的state与给定的state相与，结果不为0，则唤醒
        while let Some(to_wakeup) = guard.wait_list.pop_front() {
            if (to_wakeup.state & state) != 0 {
                unsafe {
                    process_wakeup(to_wakeup);
                }
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
