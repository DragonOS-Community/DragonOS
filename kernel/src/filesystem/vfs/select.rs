use alloc::{sync::Arc, vec::Vec};

use crate::{
    libs::wait_queue::WaitQueue,
    process::{ProcessControlBlock, ProcessManager},
    time::timekeep::ktime_t,
};

use super::{file::File, PollStatus};

/// @brief PollWqueues 对外的接口
pub trait PollTable {
    /// @brief 文件的 poll() 函数内部会调用该函数使得当前进程挂载到对应的等待队列
    fn poll_wait(&mut self, file: Arc<File>, wait_queue: Arc<WaitQueue>);
}

/// @brief 每个调用 select/poll 的进程都会维护一个 PollWqueues，用于轮询
struct PollWqueues {
    pt: bool,        // 用于改变 poll_wait() 的执行逻辑
    key: PollStatus, // 关注的事件
    polling_task: Arc<ProcessControlBlock>,
    triggered: bool,
    error: bool,
    poll_table: Vec<PollTableEntry>,
}

impl PollWqueues {
    /// @brief 初始化 PollWqueues 结构体，对应 Linux 的 poll_init_wait()
    fn new() -> Self {
        Self {
            pt: true,
            key: PollStatus::all(),
            polling_task: ProcessManager::current_pcb(),
            triggered: false,
            error: false,
            poll_table: vec![],
        }
    }

    /// @brief 在 poll 等待期间设置超时
    fn poll_schedule_timeout(&self, expires: ktime_t) {
        todo!()
    }
}

impl PollTable for PollWqueues {
    fn poll_wait(&mut self, file: Arc<File>, wait_queue: Arc<WaitQueue>) {
        if self.pt == false {
            return;
        }

        let entry = PollTableEntry::new(file, self.key, wait_queue);
        self.poll_table.push(entry);
        // TODO: 将当前进程加入等待队列
    }
}

impl Drop for PollWqueues {
    /// @brief 将 poll_table 的所有元素从对应的等待队列中移除
    fn drop(&mut self) {
        todo!()
    }
}

/// @brief 对应每一个 IO 监听事件
struct PollTableEntry {
    file: Arc<File>,
    key: PollStatus,
    wait_queue: Arc<WaitQueue>,
    // TODO: 等待队列项
}

impl PollTableEntry {
    fn new(file: Arc<File>, key: PollStatus, wait_queue: Arc<WaitQueue>) -> Self {
        Self {
            file,
            key,
            wait_queue,
        }
    }
}

impl Drop for PollTableEntry {
    /// @brief 将自身从对应的等待队列中移除
    fn drop(&mut self) {
        todo!()
    }
}

/// @brief 唤醒函数
fn poll_wake() {
    todo!()
}
