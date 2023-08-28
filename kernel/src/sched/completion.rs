use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    libs::{spinlock::SpinLock, wait_queue::WaitQueue},
    syscall::SystemError,
    time::timer::schedule_timeout,
};

const COMPLETE_ALL: u32 = core::u32::MAX;
const MAX_TIMEOUT: i64 = core::i64::MAX;

#[derive(Debug)]
pub struct Completion {
    inner: SpinLock<InnerCompletion>,
}

impl Completion {
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(InnerCompletion::new()),
        }
    }

    pub fn complete(&self) {
        let mut inner = self.inner.lock_irqsave();
        inner.complete()
    }

    /// @brief 永久标记done为Complete_All，并从wait_queue中删除所有节点
    pub fn complete_all(&mut self) {
        let mut inner = self.inner.lock_irqsave();
        inner.complete_all()
    }

    /// @brief 等待指定时间，超时后就返回, 同时设置pcb state为uninteruptible.
    /// @param timeout 非负整数，等待指定时间，超时后就返回/或者提前done
    pub fn wait_for_completion_timeout(&mut self, timeout: i64) -> Result<i64, SystemError> {
        let mut inner = self.inner.lock_irqsave();
        inner.wait_for_completion_timeout(timeout)
    }

    /// @brief 等待completion命令唤醒进程, 同时设置pcb state 为uninteruptible.
    pub fn wait_for_completion(&mut self) -> Result<i64, SystemError> {
        let mut inner = self.inner.lock_irqsave();
        inner.wait_for_completion()
    }

    /// @brief @brief 等待completion的完成，但是可以被中断
    pub fn wait_for_completion_interruptible(&mut self) -> Result<i64, SystemError> {
        let mut inner = self.inner.lock_irqsave();
        inner.wait_for_completion_interruptible()
    }

    pub fn wait_for_completion_interruptible_timeout(
        &mut self,
        timeout: i64,
    ) -> Result<i64, SystemError> {
        let mut inner = self.inner.lock_irqsave();
        inner.wait_for_completion_interruptible_timeout(timeout)
    }

    /// @brief @brief 尝试获取completion的一个done！如果您在wait之前加上这个函数作为判断，说不定会加快运行速度。
    ///
    /// @return true - 表示不需要wait_for_completion，并且已经获取到了一个completion(即返回true意味着done已经被 减1 )
    /// @return false - 表示当前done=0，您需要进入等待，即wait_for_completion
    pub fn try_wait_for_completion(&mut self) -> bool {
        let mut inner = self.inner.lock_irqsave();
        inner.try_wait_for_completion()
    }
    // @brief 测试一个completion是否有waiter。（即done是不是等于0）
    pub fn completion_done(&self) -> bool {
        let mut inner = self.inner.lock_irqsave();
        inner.completion_done()
    }
}
#[derive(Debug)]
pub struct InnerCompletion {
    done: u32,
    wait_queue: WaitQueue,
}

impl InnerCompletion {
    pub const fn new() -> Self {
        Self {
            done: 0,
            wait_queue: WaitQueue::INIT,
        }
    }
    /// @brief 唤醒一个wait_queue中的节点
    pub fn complete(&mut self) {
        if self.done != COMPLETE_ALL {
            self.done += 1;
        }
        self.wait_queue.wakeup(None);
    }

    /// @brief 永久标记done为Complete_All，并从wait_queue中删除所有节点
    pub fn complete_all(&mut self) {
        self.done = COMPLETE_ALL;
        self.wait_queue.wakeup_all(None);
    }

    /// @brief 基本函数：通用的处理wait命令的函数(即所有wait_for_completion函数最核心部分在这里)
    ///
    /// @param timeout 非负整数
    /// @param interuptible 设置进程是否能被打断
    /// @return 返回剩余时间或者SystemError
    fn do_wait_for_common(
        &mut self,
        mut timeout: i64,
        interuptible: bool,
    ) -> Result<i64, SystemError> {
        let mut irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        if self.done == 0 {
            //loop break 类似 do while 保证进行一次信号检测
            loop {
                //检查当前线程是否有未处理的信号
                //             if (signal_pending_state(state, current)) {
                // timeout = -ERESTARTSYS;
                // break;
                //}
                drop(irq_guard);
                if interuptible {
                    self.wait_queue.sleep();
                } else {
                    self.wait_queue.sleep_uninterruptible();
                }
                timeout = schedule_timeout(timeout)?;
                irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
                if self.done != 0 || timeout <= 0 {
                    break;
                }
            }
            self.wait_queue.wakeup(None);
            if self.done == 0 {
                drop(irq_guard);
                return Ok(timeout);
            }
        }
        if self.done != COMPLETE_ALL {
            self.done -= 1;
        }
        drop(irq_guard);
        return Ok(if timeout > 0 { timeout } else { 1 });
    }

    /// @brief 等待指定时间，超时后就返回, 同时设置pcb state为uninteruptible.
    /// @param timeout 非负整数，等待指定时间，超时后就返回/或者提前done
    pub fn wait_for_completion_timeout(&mut self, timeout: i64) -> Result<i64, SystemError> {
        self.do_wait_for_common(timeout, false)
    }

    /// @brief 等待completion命令唤醒进程, 同时设置pcb state 为uninteruptible.
    pub fn wait_for_completion(&mut self) -> Result<i64, SystemError> {
        self.do_wait_for_common(MAX_TIMEOUT, false)
    }

    /// @brief @brief 等待completion的完成，但是可以被中断
    pub fn wait_for_completion_interruptible(&mut self) -> Result<i64, SystemError> {
        self.do_wait_for_common(MAX_TIMEOUT, true)
    }

    pub fn wait_for_completion_interruptible_timeout(
        &mut self,
        timeout: i64,
    ) -> Result<i64, SystemError> {
        assert!(timeout >= 0);
        self.do_wait_for_common(timeout, true)
    }

    /// @brief @brief 尝试获取completion的一个done！如果您在wait之前加上这个函数作为判断，说不定会加快运行速度。
    ///
    /// @return true - 表示不需要wait_for_completion，并且已经获取到了一个completion(即返回true意味着done已经被 减1 )
    /// @return false - 表示当前done=0，您需要进入等待，即wait_for_completion
    pub fn try_wait_for_completion(&mut self) -> bool {
        if self.done == 0 {
            return false;
        }

        if self.done != 0 {
            return false;
        } else if self.done != COMPLETE_ALL {
            self.done -= 1;
        }

        return true;
    }

    // @brief 测试一个completion是否有waiter。（即done是不是等于0）
    pub fn completion_done(&self) -> bool {
        if self.done == 0 {
            return false;
        }

        if self.done == 0 {
            return false;
        }
        return true;
        // 脱离生命周期，自动释放guard
    }
}
