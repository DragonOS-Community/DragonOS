use crate::{
    kwarn,
    libs::{rwlock::RwLock, spinlock::SpinLock},
    syscall::SystemError,
};
use alloc::{sync::Arc, vec::Vec};
use core::ffi::c_void;

/// @brief 通知链中注册的回调函数类型
type NotifierFnT = fn(Arc<NotifierBlock>, u64, *mut c_void) -> i32;

#[derive(Debug)]
/// @brief 通知链节点
pub struct NotifierBlock {
    notifier_call: Option<NotifierFnT>,
    priority: i32,
}

impl NotifierBlock {
    pub fn new(notifier_call: Option<NotifierFnT>, priority: i32) -> Self {
        Self {
            notifier_call,
            priority,
        }
    }
}

/// @brief 通知链
struct NotifierChain(Vec<Arc<NotifierBlock>>);

impl NotifierChain {
    pub fn new() -> Self {
        Self(vec![])
    }

    /// @brief 将节点注册到通知链
    /// @param unique_priority 检查优先级的唯一性
    // TODO: 未加入 RCU 锁的操作？
    pub fn register(
        &mut self,
        block: Arc<NotifierBlock>,
        unique_priority: bool,
    ) -> Result<(), SystemError> {
        let mut index: usize = 0;

        // 在 notifier chain中寻找第一个优先级比要插入块低的块
        for b in self.0.iter() {
            // 判断之前是否已经注册过该节点
            if Arc::as_ptr(&block) == Arc::as_ptr(b) {
                kwarn!(
                    "notifier callback {:?} already registered",
                    block.notifier_call
                );
                return Err(SystemError::EEXIST);
            }

            if block.priority > b.priority {
                break;
            }

            // 优先级唯一性检测
            if block.priority == b.priority && unique_priority {
                return Err(SystemError::EBUSY);
            }

            index += 1;
        }

        // 插入 notifier chain
        self.0.insert(index, block);
        return Ok(());
    }

    /// @brief 在通知链中取消注册节点
    // TODO: 未加入 RCU 锁的操作？
    pub fn unregister(&mut self, block: Arc<NotifierBlock>) -> Result<(), SystemError> {
        let mut index: usize = 0;

        // 在 notifier chain 中寻找要删除的节点
        for b in self.0.iter() {
            if Arc::as_ptr(&block) == Arc::as_ptr(b) {
                // 在 notifier chain 中删除
                self.0.remove(index);
                return Ok(());
            }
            index += 1;
        }
        return Err(SystemError::ENOENT);
    }

    /// @brief 通知链进行事件通知
    /// @param nr_to_call 回调函数次数，如果该参数小于 0，则忽略
    /// @param nr_calls 记录回调函数次数，如果该参数为空指针，则忽略
    /// return 返回最后一次回调函数的返回值
    // TODO: 未加入 RCU 锁的操作？
    // TODO: 增加 NOTIFIER_STOP_MASK 相关功能
    pub fn call_chain(&self, val: u64, v: *mut c_void, nr_to_call: i32, nr_calls: *mut i32) -> i32 {
        if !nr_calls.is_null() {
            unsafe {
                *nr_calls = 0;
            }
        }
        let mut nr_to_call = nr_to_call;
        let mut ret: i32 = 0;

        for b in self.0.iter() {
            if nr_to_call == 0 {
                break;
            }

            if let Some(notifier_call) = b.notifier_call {
                ret = notifier_call(b.clone(), val, v);
            }

            if !nr_calls.is_null() {
                unsafe {
                    *nr_calls += 1;
                }
            }

            nr_to_call -= 1;
        }
        return ret;
    }
}

/// @brief 原子的通知链，使用 SpinLock 进行同步
pub struct AtomicNotifierChain(SpinLock<NotifierChain>);

impl AtomicNotifierChain {
    pub fn new() -> Self {
        Self(SpinLock::new(NotifierChain::new()))
    }

    pub fn register(
        &mut self,
        block: Arc<NotifierBlock>,
        unique_priority: bool,
    ) -> Result<(), SystemError> {
        let mut notifier_chain_guard = self.0.lock();
        return notifier_chain_guard.register(block, unique_priority);
    }

    pub fn unregister(&mut self, block: Arc<NotifierBlock>) -> Result<(), SystemError> {
        let mut notifier_chain_guard = self.0.lock();
        return notifier_chain_guard.unregister(block);
    }

    pub fn call_chain(&self, val: u64, v: *mut c_void, nr_to_call: i32, nr_calls: *mut i32) -> i32 {
        let notifier_chain_guard = self.0.lock();
        return notifier_chain_guard.call_chain(val, v, nr_to_call, nr_calls);
    }
}

/// @brief 可阻塞的通知链，使用 RwLock 进行同步
pub struct BlockingNotifierChain(RwLock<NotifierChain>);

impl BlockingNotifierChain {
    pub fn new() -> Self {
        Self(RwLock::new(NotifierChain::new()))
    }

    pub fn register(
        &mut self,
        block: Arc<NotifierBlock>,
        unique_priority: bool,
    ) -> Result<(), SystemError> {
        let mut notifier_chain_guard = self.0.write();
        return notifier_chain_guard.register(block, unique_priority);
    }

    pub fn unregister(&mut self, block: Arc<NotifierBlock>) -> Result<(), SystemError> {
        let mut notifier_chain_guard = self.0.write();
        return notifier_chain_guard.unregister(block);
    }

    pub fn call_chain(&self, val: u64, v: *mut c_void, nr_to_call: i32, nr_calls: *mut i32) -> i32 {
        let notifier_chain_guard = self.0.read();
        return notifier_chain_guard.call_chain(val, v, nr_to_call, nr_calls);
    }
}

/// @brief 原始的通知链，由调用者自行考虑同步
pub struct RawNotifierChain(NotifierChain);
