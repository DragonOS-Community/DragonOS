use core::sync::atomic::{AtomicU32, Ordering};

use crate::{
    arch::{
        asm::irqflags::{local_irq_restore, local_irq_save},
        CurrentIrqArch,
    },
    exception::InterruptArch,
    libs::wait_queue::WaitQueue,
    syscall::SystemError,
    time::timer::schedule_timeout,
};

// void completion_init(struct completion *x);
// void complete(struct completion *x);
// void complete_all(struct completion *x);
// void wait_for_completion(struct completion *x);
// long wait_for_completion_timeout(struct completion *x, long timeout);
// void wait_for_completion_interruptible(struct completion *x);
// long wait_for_completion_interruptible_timeout(struct completion *x, long timeout);
// void wait_for_multicompletion(struct completion x[], int n);
// bool try_wait_for_completion(struct completion *x);
// bool completion_done(struct completion *x);
// struct completion *completion_alloc();

const COMPLETE_ALL: u32 = core::u32::MAX;
const MAX_TIMEOUT: i64 = core::i64::MAX;

#[derive(Debug)]
struct Completion {
    done: AtomicU32,
    wait_queue: WaitQueue,
}

impl Completion {
    pub const fn new() -> Self {
        Self {
            done: AtomicU32::new(0),
            wait_queue: WaitQueue::INIT,
        }
    }
    /// @brief 唤醒一个wait_queue中的节点
    pub fn complete(&self) {
        let guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        if self.done.load(Ordering::SeqCst) != COMPLETE_ALL {
            self.done.fetch_add(1, Ordering::SeqCst);
        }
        self.wait_queue.wakeup(None);
        drop(guard);
    }

    /// @brief 永久标记done为Complete_All，并从wait_queue中删除所有节点
    pub fn complete_all(&self) {
        let guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        self.done.store(COMPLETE_ALL, Ordering::SeqCst);
        self.wait_queue.wakeup_all(None);
        drop(guard);
    }

    /// @brief 基本函数：通用的处理wait命令的函数(即所有wait_for_completion函数最核心部分在这里)
    ///
    /// @param timeout 非负整数
    /// @param interuptible 设置进程是否能被打断
    /// @return 返回剩余时间或者SystemError
    fn do_wait_for_common(&self, mut timeout: i64, interuptible: bool) -> Result<i64, SystemError> {
        let mut flags = local_irq_save();
        if self.done.load(Ordering::SeqCst) != 0 {
            //loop break 类似 do while 保证进行一次信号检测
            loop {
                //检查当前！线程！是否有未处理的信号
                //             if (signal_pending_state(state, current)) {
                // timeout = -ERESTARTSYS;
                // break;
                //}
                local_irq_restore(flags);
                if interuptible {
                    self.wait_queue.sleep();
                } else {
                    self.wait_queue.sleep_uninterruptible();
                }
                //TODO 考虑是否真的需要SystemError
                timeout = schedule_timeout(timeout)?;
                flags = local_irq_save();
                if self.done.load(Ordering::SeqCst) == 0 || timeout <= 0 {
                    break;
                }
            }
            self.wait_queue.wakeup(None);
            if self.done.load(Ordering::SeqCst) > 0 {
                return Ok(timeout);
            }
        }
        if self.done.load(Ordering::SeqCst) > 0 && self.done.load(Ordering::SeqCst) != COMPLETE_ALL
        {
            self.done.fetch_sub(1, Ordering::SeqCst);
        }
        return Ok(if timeout > 0 { timeout } else { 1 });
    }

    /// @brief 等待指定时间，超时后就返回, 同时设置pcb state为uninteruptible.
    /// @param timeout 非负整数，等待指定时间，超时后就返回/或者提前done
    pub fn wait_for_completion_timeout(&self, timeout: i64) -> Result<i64, SystemError> {
        self.do_wait_for_common(timeout, false)
    }

    /// @brief 等待completion命令唤醒进程, 同时设置pcb state 为uninteruptible.
    pub fn wait_for_completion(&self) -> Result<i64, SystemError> {
        self.do_wait_for_common(MAX_TIMEOUT, false)
    }

    /// @brief @brief 等待completion的完成，但是可以被中断
    pub fn wait_for_completion_interruptible(&self) -> Result<i64, SystemError> {
        self.do_wait_for_common(MAX_TIMEOUT, true)
    }

    pub fn wait_for_completion_interruptible_timeout(
        &self,
        timeout: i64,
    ) -> Result<i64, SystemError> {
        assert!(timeout >= 0);
        self.do_wait_for_common(timeout, true)
    }

    pub fn try_wait_for_completion(&self) -> bool {
        if self.done.load(Ordering::SeqCst) == 0 {
            return false;
        }
        let guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        if self.done.load(Ordering::SeqCst) != 0 {
            return false;
        } else if self.done.load(Ordering::SeqCst) != COMPLETE_ALL {
            self.done.fetch_sub(1, Ordering::SeqCst);
        }
        drop(guard);
        return true;
    }
}
