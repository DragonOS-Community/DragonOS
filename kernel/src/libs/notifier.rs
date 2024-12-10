#![allow(dead_code)]
use core::fmt::Debug;

use crate::libs::{rwlock::RwLock, spinlock::SpinLock};
use alloc::{sync::Arc, vec::Vec};
use log::warn;
use system_error::SystemError;

/// @brief 通知链节点
pub trait NotifierBlock<V: Clone + Copy, T>: Debug + Send + Sync {
    /// @brief 通知链中注册的回调函数类型
    fn notifier_call(&self, action: V, data: Option<&T>) -> i32;
    /// @brief 通知链节点的优先级
    fn priority(&self) -> i32;
}

/// @brief 通知链
// TODO: 考虑使用红黑树封装
#[derive(Debug)]
struct NotifierChain<V: Clone + Copy, T>(Vec<Arc<dyn NotifierBlock<V, T>>>);

impl<V: Clone + Copy, T> NotifierChain<V, T> {
    pub fn new() -> Self {
        Self(vec![])
    }

    /// @brief 将节点注册到通知链
    /// @param unique_priority 检查通知链中优先级的唯一性
    pub fn register(
        &mut self,
        block: Arc<dyn NotifierBlock<V, T>>,
        unique_priority: bool,
    ) -> Result<(), SystemError> {
        let mut index: usize = 0;

        // 在 notifier chain中寻找第一个优先级比要插入块低的块
        for b in self.0.iter() {
            // 判断之前是否已经注册过该节点
            if Arc::ptr_eq(&block, b) {
                warn!(
                    "notifier callback {:?} already registered",
                    Arc::as_ptr(&block)
                );
                return Err(SystemError::EEXIST);
            }

            if block.priority() > b.priority() {
                break;
            }

            // 优先级唯一性检测
            if block.priority() == b.priority() && unique_priority {
                return Err(SystemError::EBUSY);
            }

            index += 1;
        }

        // 插入 notifier chain
        self.0.insert(index, block);
        return Ok(());
    }

    /// @brief 在通知链中取消注册节点
    pub fn unregister(&mut self, block: Arc<dyn NotifierBlock<V, T>>) -> Result<(), SystemError> {
        let remove = self.0.extract_if(|b| Arc::ptr_eq(&block, b));
        match remove.count() {
            0 => return Err(SystemError::ENOENT),
            _ => return Ok(()),
        }
    }

    /// 通知链进行事件通知
    ///
    /// ## 参数
    ///
    /// - nr_to_call 最大调用回调函数的数量，如果为None，则不限制次数
    ///
    /// ## 返回
    ///
    /// (最后一次回调函数的返回值，回调次数)
    ///
    /// TODO: 增加 NOTIFIER_STOP_MASK 相关功能
    pub fn call_chain(
        &self,
        action: V,
        data: Option<&T>,
        nr_to_call: Option<usize>,
    ) -> (i32, usize) {
        let mut ret: i32 = 0;
        let mut nr_calls: usize = 0;

        for b in self.0.iter() {
            if nr_to_call.is_some_and(|x| nr_calls >= x) {
                break;
            }
            ret = b.notifier_call(action, data);
            nr_calls += 1;
        }
        return (ret, nr_calls);
    }
}

/// @brief 原子的通知链，使用 SpinLock 进行同步
#[derive(Debug)]
pub struct AtomicNotifierChain<V: Clone + Copy, T>(SpinLock<NotifierChain<V, T>>);

impl<V: Clone + Copy, T> Default for AtomicNotifierChain<V, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V: Clone + Copy, T> AtomicNotifierChain<V, T> {
    pub fn new() -> Self {
        Self(SpinLock::new(NotifierChain::<V, T>::new()))
    }

    pub fn register(&mut self, block: Arc<dyn NotifierBlock<V, T>>) -> Result<(), SystemError> {
        let mut notifier_chain_guard = self.0.lock();
        return notifier_chain_guard.register(block, false);
    }

    pub fn register_unique_prio(
        &mut self,
        block: Arc<dyn NotifierBlock<V, T>>,
    ) -> Result<(), SystemError> {
        let mut notifier_chain_guard = self.0.lock();
        return notifier_chain_guard.register(block, true);
    }

    pub fn unregister(&mut self, block: Arc<dyn NotifierBlock<V, T>>) -> Result<(), SystemError> {
        let mut notifier_chain_guard = self.0.lock();
        return notifier_chain_guard.unregister(block);
    }

    pub fn call_chain(
        &self,
        action: V,
        data: Option<&T>,
        nr_to_call: Option<usize>,
    ) -> (i32, usize) {
        let notifier_chain_guard = self.0.lock();
        return notifier_chain_guard.call_chain(action, data, nr_to_call);
    }
}

/// @brief 可阻塞的通知链，使用 RwLock 进行同步
// TODO: 使用 semaphore 封装
#[derive(Debug)]
pub struct BlockingNotifierChain<V: Clone + Copy, T>(RwLock<NotifierChain<V, T>>);

impl<V: Clone + Copy, T> BlockingNotifierChain<V, T> {
    pub fn new() -> Self {
        Self(RwLock::new(NotifierChain::<V, T>::new()))
    }

    pub fn register(&mut self, block: Arc<dyn NotifierBlock<V, T>>) -> Result<(), SystemError> {
        let mut notifier_chain_guard = self.0.write();
        return notifier_chain_guard.register(block, false);
    }

    pub fn register_unique_prio(
        &mut self,
        block: Arc<dyn NotifierBlock<V, T>>,
    ) -> Result<(), SystemError> {
        let mut notifier_chain_guard = self.0.write();
        return notifier_chain_guard.register(block, true);
    }

    pub fn unregister(&mut self, block: Arc<dyn NotifierBlock<V, T>>) -> Result<(), SystemError> {
        let mut notifier_chain_guard = self.0.write();
        return notifier_chain_guard.unregister(block);
    }

    pub fn call_chain(
        &self,
        action: V,
        data: Option<&T>,
        nr_to_call: Option<usize>,
    ) -> (i32, usize) {
        let notifier_chain_guard = self.0.read();
        return notifier_chain_guard.call_chain(action, data, nr_to_call);
    }
}

/// @brief 原始的通知链，由调用者自行考虑同步
pub struct RawNotifierChain<V: Clone + Copy, T>(NotifierChain<V, T>);

impl<V: Clone + Copy, T> RawNotifierChain<V, T> {
    pub fn new() -> Self {
        Self(NotifierChain::<V, T>::new())
    }

    pub fn register(&mut self, block: Arc<dyn NotifierBlock<V, T>>) -> Result<(), SystemError> {
        return self.0.register(block, false);
    }

    pub fn unregister(&mut self, block: Arc<dyn NotifierBlock<V, T>>) -> Result<(), SystemError> {
        return self.0.unregister(block);
    }

    pub fn call_chain(
        &self,
        action: V,
        data: Option<&T>,
        nr_to_call: Option<usize>,
    ) -> (i32, usize) {
        return self.0.call_chain(action, data, nr_to_call);
    }
}
