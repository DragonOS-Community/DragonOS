#![allow(dead_code)]
use core::fmt::Debug;

use alloc::{sync::Arc, vec::Vec};
use log::warn;
use system_error::SystemError;

use crate::{
    libs::{rwlock::RwLock, spinlock::SpinLock},
    rcu::{rcu_read_lock_held, synchronize_rcu, RcuArcSlot},
};

bitflags! {
    /// Linux notifier callback return values.
    pub struct NotifyResult: i32 {
        const DONE = 0x0000;
        const OK = 0x0001;
        const STOP_MASK = 0x8000;
        const BAD = Self::STOP_MASK.bits | 0x0002;
        const STOP = Self::OK.bits | Self::STOP_MASK.bits;
    }
}

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

impl<V: Clone + Copy, T> Clone for NotifierChain<V, T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

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
        let remove = self.0.extract_if(.., |b| Arc::ptr_eq(&block, b));
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
            if NotifyResult::from_bits_truncate(ret).contains(NotifyResult::STOP_MASK) {
                break;
            }
        }
        return (ret, nr_calls);
    }
}

/// @brief 原子的通知链，更新侧使用 SpinLock 串行化，调用侧使用 RCU 快照遍历
#[derive(Debug)]
pub struct AtomicNotifierChain<V, T>
where
    V: Clone + Copy + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    update_lock: SpinLock<()>,
    chain: RcuArcSlot<NotifierChain<V, T>>,
}

impl<V, T> Default for AtomicNotifierChain<V, T>
where
    V: Clone + Copy + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<V, T> AtomicNotifierChain<V, T>
where
    V: Clone + Copy + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self {
            update_lock: SpinLock::new(()),
            chain: RcuArcSlot::new(Arc::new(NotifierChain::<V, T>::new())),
        }
    }

    pub fn register(&self, block: Arc<dyn NotifierBlock<V, T>>) -> Result<(), SystemError> {
        let _guard = self.update_lock.lock_irqsave();
        let mut new_chain = (*self.chain.load()).clone();
        new_chain.register(block, false)?;
        self.chain.store_deferred(Arc::new(new_chain));
        return Ok(());
    }

    pub fn register_unique_prio(
        &self,
        block: Arc<dyn NotifierBlock<V, T>>,
    ) -> Result<(), SystemError> {
        let _guard = self.update_lock.lock_irqsave();
        let mut new_chain = (*self.chain.load()).clone();
        new_chain.register(block, true)?;
        self.chain.store_deferred(Arc::new(new_chain));
        return Ok(());
    }

    pub fn unregister(&self, block: Arc<dyn NotifierBlock<V, T>>) -> Result<(), SystemError> {
        if rcu_read_lock_held() {
            warn!("atomic notifier unregister called from an RCU read-side section");
            return Err(SystemError::EDEADLK_OR_EDEADLOCK);
        }

        {
            let _guard = self.update_lock.lock_irqsave();
            let mut new_chain = (*self.chain.load()).clone();
            new_chain.unregister(block)?;
            self.chain.store_deferred(Arc::new(new_chain));
        }

        synchronize_rcu();
        return Ok(());
    }

    pub fn call_chain(
        &self,
        action: V,
        data: Option<&T>,
        nr_to_call: Option<usize>,
    ) -> (i32, usize) {
        return self
            .chain
            .with_read(|chain| chain.call_chain(action, data, nr_to_call));
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
