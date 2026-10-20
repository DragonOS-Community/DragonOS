use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

use crate::{context::Action, scheduler::TID_EID};

use super::{SchedEntity, Scheduler};

// 最大线程数
pub const MAX_THREAD_NUM: usize = 32;
// 默认线程数
pub const DEFAULT_THREAD_NUM: usize = 2;

lazy_static! {
    // 全局任务队列
    pub static ref TASK_DEQUE: Mutex<TaskDeque> = Mutex::new(TaskDeque {
        max_num: DEFAULT_THREAD_NUM,
        queue: Vec::new(),
    });
}

/// # 任务队列
pub struct TaskDeque {
    max_num: usize,
    queue: Vec<JoinHandle<()>>,
}

impl TaskDeque {
    /// 将构建或安装DADK任务添加到任务队列中
    ///
    /// ## 参数
    ///
    /// - `action` : 要执行的操作
    /// - `dragonos_dir` : DragonOS sysroot在主机上的路径
    /// - `entity` : 任务实体
    ///
    /// ## 返回值
    ///
    /// true 任务添加成功
    /// false 任务添加失败
    pub fn build_install_task(
        &mut self,
        action: Action,
        dragonos_dir: PathBuf,
        entity: Arc<SchedEntity>,
    ) -> bool {
        // log::warn!("push stack: task:{} {entity:?}", entity.id());
        if self.queue.len() < self.max_num {
            let id = entity.id();
            let handler = std::thread::spawn(move || {
                Scheduler::execute(action, dragonos_dir.clone(), entity)
            });
            TID_EID.lock().unwrap().insert(handler.thread().id(), id);
            self.queue.push(handler);
            return true;
        }
        return false;
    }

    /// 将清理DADK任务添加到任务队列中
    ///
    /// ## 参数
    ///
    /// - `action` : 要执行的操作
    /// - `dragonos_dir` : DragonOS sysroot在主机上的路径
    /// - `entity` : 任务实体
    ///
    /// ## 返回值
    ///
    /// 无
    pub fn clean_task(&mut self, action: Action, dragonos_dir: PathBuf, entity: Arc<SchedEntity>) {
        while self.queue.len() >= self.max_num {
            self.queue.retain(|x| !x.is_finished());
        }
        let handler =
            std::thread::spawn(move || Scheduler::execute(action, dragonos_dir.clone(), entity));
        self.queue.push(handler);
    }

    pub fn queue(&self) -> &Vec<JoinHandle<()>> {
        return &self.queue;
    }

    pub fn queue_mut(&mut self) -> &mut Vec<JoinHandle<()>> {
        return &mut self.queue;
    }

    pub fn set_thread(&mut self, mut thread: usize) {
        if thread > MAX_THREAD_NUM {
            thread = MAX_THREAD_NUM;
        }
        self.max_num = thread;
    }
}
