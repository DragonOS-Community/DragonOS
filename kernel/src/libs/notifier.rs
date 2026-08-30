#![allow(dead_code)]
use core::fmt::Debug;

use alloc::{sync::Arc, vec::Vec};
use log::warn;
use system_error::SystemError;

use crate::{
    libs::{mutex::Mutex, rwlock::RwLock, spinlock::SpinLock},
    rcu::{
        rcu_read_lock_held,
        srcu::{SrcuArcSlot, SrcuDomain},
        synchronize_rcu, RcuArcSlot,
    },
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

        self.0.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        // 插入 notifier chain
        self.0.insert(index, block);
        return Ok(());
    }

    fn try_clone(&self) -> Result<Self, SystemError> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(self.0.len())
            .map_err(|_| SystemError::ENOMEM)?;
        entries.extend(self.0.iter().cloned());
        Ok(Self(entries))
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

/// A sleepable notifier chain whose readers are protected by a private SRCU
/// domain. Callbacks run without the update mutex and may block.
pub struct SrcuNotifierChain<V, T>
where
    V: Clone + Copy + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    update_lock: Mutex<()>,
    domain: SrcuDomain,
    chain: SrcuArcSlot<NotifierChain<V, T>>,
}

impl<V, T> core::fmt::Debug for SrcuNotifierChain<V, T>
where
    V: Clone + Copy + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SrcuNotifierChain")
            .field("domain", &self.domain)
            .finish_non_exhaustive()
    }
}

impl<V, T> SrcuNotifierChain<V, T>
where
    V: Clone + Copy + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    pub fn try_new(name: &'static str) -> Result<Self, SystemError> {
        Ok(Self {
            update_lock: Mutex::new(()),
            domain: SrcuDomain::try_new(name)?,
            chain: SrcuArcSlot::new(
                Arc::try_new(NotifierChain::new()).map_err(|_| SystemError::ENOMEM)?,
            ),
        })
    }

    fn update_and_synchronize(
        &self,
        change: impl FnOnce(&mut NotifierChain<V, T>) -> Result<(), SystemError>,
    ) -> Result<(), SystemError> {
        self.domain.validate_update_context()?;
        let old = {
            let _update = self.update_lock.lock();
            let mut new_chain = self
                .chain
                .with_read(&self.domain, NotifierChain::try_clone)?;
            change(&mut new_chain)?;
            let new_chain = Arc::try_new(new_chain).map_err(|_| SystemError::ENOMEM)?;
            // SAFETY: this mutex serializes writers; the following GP protects old readers.
            unsafe { self.chain.swap(new_chain) }
        };
        self.domain.synchronize_after_publication();
        drop(old);
        Ok(())
    }

    pub fn register(&self, block: Arc<dyn NotifierBlock<V, T>>) -> Result<(), SystemError> {
        let retained = block.clone();
        let result = self.update_and_synchronize(|chain| chain.register(block, false));
        drop(retained);
        result
    }

    pub fn register_unique_prio(
        &self,
        block: Arc<dyn NotifierBlock<V, T>>,
    ) -> Result<(), SystemError> {
        let retained = block.clone();
        let result = self.update_and_synchronize(|chain| chain.register(block, true));
        drop(retained);
        result
    }

    pub fn unregister(&self, block: Arc<dyn NotifierBlock<V, T>>) -> Result<(), SystemError> {
        let retained = block.clone();
        let result = self.update_and_synchronize(|chain| chain.unregister(block));
        drop(retained);
        result
    }

    pub fn call_chain(
        &self,
        action: V,
        data: Option<&T>,
        nr_to_call: Option<usize>,
    ) -> (i32, usize) {
        self.chain.with_read(&self.domain, |chain| {
            chain.call_chain(action, data, nr_to_call)
        })
    }

    /// Consumes an unused chain and unregisters its private SRCU domain.
    pub fn try_cleanup(self) -> Result<(), (Self, SystemError)> {
        if let Err(error) = self.domain.barrier() {
            return Err((self, error));
        }
        // SAFETY: consuming the chain removes every public path to its private domain.
        match unsafe { self.domain.try_cleanup_in_place() } {
            Ok(()) => Ok(()),
            Err(error) => Err((self, error)),
        }
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
