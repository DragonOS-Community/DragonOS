use alloc::{collections::LinkedList, vec::Vec};
use crate::{
    arch::{asm::current::current_pcb, sched::sched},
    process::{ProcessControlBlock,ProcessState},
    include::bindings::bindings::process_wakeup,
    libs::{spinlock::SpinLock,spinlock::SpinLockGuard},
};


#[derive(Debug)]
struct InnerWaitQueue {
    /// 等待队列的链表
    wait_list: LinkedList<&'static mut ProcessControlBlock>,
}

impl InnerWaitQueue {
    pub const INIT: InnerWaitQueue = InnerWaitQueue {
        wait_list: LinkedList::new(),
    };
}

/// 被自旋锁保护的等待队列
#[derive(Debug)]
pub struct WaitQueue(SpinLock<InnerWaitQueue>);

impl WaitQueue {
    pub const INIT: WaitQueue = WaitQueue(SpinLock::new(InnerWaitQueue::INIT));

    /// @brief 让当前进程在等待队列上进行等待，允许被信号打断
    pub fn sleep_on_interruptible(&self) {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        current_pcb().inner.write().state = ProcessState::Blocked(true);
        guard.wait_list.push_back(current_pcb());
        drop(guard);

        sched();
    }

    /// @brief 让当前进程在等待队列上进行等待，并且，不允许被信号打断
    pub fn sleep_on_uninterruptible(&self) {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        current_pcb().inner.write().state = ProcessState::Blocked(false);
        guard.wait_list.push_back(current_pcb());
        drop(guard);

        sched();
    }

    /// @brief 在等待队列上进行等待，同时释放自旋锁，不允许打断。
    pub fn sleep_uninterruptible_unlock_spinlock<T>(&self, to_unlock: SpinLockGuard<T>) {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        current_pcb().inner.write().state = ProcessState::Blocked(false);
        guard.wait_list.push_back(current_pcb());
        drop(to_unlock);
        drop(guard);

        sched();
    }

    /// @brief 唤醒在队列中等待的第一个进程。
    ///
    /// @param state 用于判断的state，如果队列中第一个进程的state与相等,则唤醒这个进程。
    ///
    /// @return true 成功唤醒进程
    /// @return false 没有唤醒进程
    pub fn wakeup(&self, state: ProcessState) -> bool {
        let mut guard: SpinLockGuard<InnerWaitQueue> = self.0.lock();
        // 如果队列为空，则返回
        if guard.wait_list.is_empty() {
            return false;
        }       
        if guard.wait_list.front().unwrap().inner.read().state==state {
            let to_wakeup = guard.wait_list.pop_front().unwrap();
            unsafe {
                process_wakeup(to_wakeup);
            }
            return true;
        } else {
            return false;
        }
    }

    /// @brief 获得当前等待队列的大小
    pub fn len(&self) -> usize {
        return self.0.lock().wait_list.len();
    }
}

