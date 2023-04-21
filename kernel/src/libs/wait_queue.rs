#![allow(dead_code)]
use alloc::{collections::LinkedList, vec::Vec};

use crate::{
    arch::{asm::current::current_pcb, sched::sched},
    include::bindings::bindings::{
        process_control_block, process_wakeup, wait_queue_head_t, PROC_INTERRUPTIBLE,
        PROC_UNINTERRUPTIBLE,
    },
};

use super::{
    list::list_init,
    mutex::MutexGuard,
    spinlock::{SpinLock, SpinLockGuard},
};

impl Default for wait_queue_head_t {
    fn default() -> Self {
        let mut x = Self {
            wait_list: Default::default(),
            lock: Default::default(),
        };
        list_init(&mut x.wait_list);
        return x;
    }
}

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
