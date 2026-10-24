use crate::{
    arch::ipc::signal::{SigFlags, SigSet, Signal, MAX_SIG_NUM},
    cgroup::memory::{accounting::KernelByteKind, object::ObjectAccount, owned::OwnedBox},
    filesystem::epoll::event_poll::EPollItemList,
    ipc::signal_types::{SaHandlerType, Sigaction, SigactionType, SIG_KERNEL_IGNORE_MASK},
    libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
    libs::wait_queue::WaitQueue,
    mm::allocation::MemoryAllocationError,
    process::{ProcessControlBlock, ProcessManager},
};
use alloc::{sync::Arc, vec::Vec};
use core::{fmt::Debug, sync::atomic::compiler_fence};
use system_error::SystemError;

/// Signal dispositions may be shared by distinct processes using CLONE_SIGHAND.
/// Process-directed signals and lifecycle state live in ProcessSignalState.
pub struct SigHand {
    body: OwnedBox<SigHandData>,
}

#[derive(Debug)]
struct SigHandData {
    inner: RwLock<InnerSigHand>,
    signalfd_wqh: WaitQueue,
    signalfd_epitems: EPollItemList,
}

impl Debug for SigHand {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SigHand").finish()
    }
}

#[derive(Debug)]
pub struct InnerSigHand {
    pub handlers: [Sigaction; MAX_SIG_NUM],
    pub cnt: i64,
}

impl SigHand {
    pub(crate) fn try_new_for(
        creator: &Arc<ObjectAccount>,
    ) -> Result<Arc<Self>, MemoryAllocationError> {
        let storage = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_allocate()
            .map_err(|_| SystemError::ENOMEM)?;
        let body = storage.init(SigHandData {
            inner: RwLock::new(InnerSigHand {
                handlers: default_sighandlers(),
                cnt: 0,
            }),
            signalfd_wqh: WaitQueue::default(),
            signalfd_epitems: EPollItemList::default(),
        });
        Ok(Arc::try_new(Self { body }).map_err(|_| SystemError::ENOMEM)?)
    }
    fn inner(&self) -> RwLockReadGuard<'_, InnerSigHand> {
        self.body.inner.read_irqsave()
    }
    fn inner_mut(&self) -> RwLockWriteGuard<'_, InnerSigHand> {
        self.body.inner.write_irqsave()
    }
    pub fn inner_read(&self) -> RwLockReadGuard<'_, InnerSigHand> {
        self.inner()
    }
    pub fn signalfd_wqh(&self) -> &WaitQueue {
        &self.body.signalfd_wqh
    }
    pub fn signalfd_epitems(&self) -> &EPollItemList {
        &self.body.signalfd_epitems
    }
    pub fn attach_task_ref(&self) {
        self.inner_mut().cnt += 1;
    }
    pub fn detach_task_ref(&self) {
        let mut g = self.inner_mut();
        assert!(g.cnt > 0, "SigHand::detach_task_ref underflow");
        g.cnt -= 1;
    }
    pub fn reset_handlers(&self) {
        initialize_default_sighandlers(&mut self.inner_mut().handlers);
    }
    pub fn handler(&self, sig: Signal) -> Option<Sigaction> {
        self.inner().handlers.get(Self::sig2idx(sig)).cloned()
    }
    /// Select a disposition for delivery and consume SA_RESETHAND atomically.
    /// Like Linux get_signal(), retain the selected action for this delivery
    /// while resetting only the shared handler, including on frame setup failure.
    pub fn handler_for_delivery(&self, sig: Signal) -> Option<Sigaction> {
        let mut inner = self.inner_mut();
        let handler = inner.handlers.get_mut(Self::sig2idx(sig))?;
        let selected = *handler;
        if selected.action().is_customized() && selected.flags().contains(SigFlags::SA_RESETHAND) {
            handler.set_action(SigactionType::SaHandler(SaHandlerType::Default));
        }
        Some(selected)
    }
    pub fn set_handler(&self, sig: Signal, act: Sigaction) {
        if let Some(h) = self.inner_mut().handlers.get_mut(Self::sig2idx(sig)) {
            *h = act;
        }
    }
    fn sig2idx(sig: Signal) -> usize {
        sig as usize - 1
    }
    pub fn copy_handlers_from(&self, other: &Arc<SigHand>) {
        let other_guard = other.inner();
        let mut self_guard = self.inner_mut();
        assert_eq!(self_guard.handlers.len(), other_guard.handlers.len());
        self_guard.handlers.clone_from_slice(&other_guard.handlers);
    }
    pub fn load_count(&self) -> i64 {
        self.inner().cnt
    }
    pub fn is_shared(&self) -> bool {
        self.load_count() > 1
    }
}

fn default_sighandlers() -> [Sigaction; MAX_SIG_NUM] {
    let mut actions = [Sigaction::default(); MAX_SIG_NUM];
    initialize_default_sighandlers(&mut actions);
    actions
}

fn initialize_default_sighandlers(r: &mut [Sigaction; MAX_SIG_NUM]) {
    r.fill(Sigaction::default());
    let mut sig_ign = Sigaction::default();
    // 收到忽略的信号，重启系统调用
    // Linux ignores SIGURG/SIGWINCH by default; SIGCHLD is also ignored by default,
    // but the handler must remain SIG_DFL to distinguish default ignore from explicit SIG_IGN.
    sig_ign.set_action(SigactionType::SaHandler(SaHandlerType::Ignore));
    sig_ign.flags_mut().insert(SigFlags::SA_RESTART);

    r[Signal::SIGURG as usize - 1] = sig_ign;
    r[Signal::SIGWINCH as usize - 1] = sig_ign;
}

impl SigHand {
    /// 刷新指定进程的sighand的sigaction，将满足条件的sigaction恢复为默认状态。
    /// 除非某个信号被设置为忽略且 `force_default` 为 `false`，否则都不会将其恢复。
    ///
    /// # 参数
    ///
    /// - `pcb`: 要被刷新的pcb。
    /// - `force_default`: 是否强制将sigaction恢复成默认状态。
    pub(crate) fn flush_handlers(&self, force_default: bool) {
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // debug!("hand=0x{:018x}", hand as *const sighand_struct as usize);
        let actions = &mut self.inner_mut().handlers;

        for sigaction in actions.iter_mut() {
            if force_default || !sigaction.is_ignore() {
                sigaction.set_action(SigactionType::SaHandler(SaHandlerType::Default));
            }
            // 清除flags中，除了DFL和IGN以外的所有标志
            sigaction.set_restorer(None);
            *sigaction.mask_mut() = SigSet::empty();
            *sigaction.flags_mut() = SigFlags::empty();
            compiler_fence(core::sync::atomic::Ordering::SeqCst);
        }
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
}

impl ProcessControlBlock {
    pub fn flush_signal_handlers(&self, force_default: bool) {
        self.sighand().flush_handlers(force_default);
    }
}

pub(super) fn do_sigaction(
    sig: Signal,
    act: Option<&mut Sigaction>,
    old_act: Option<&mut Sigaction>,
) -> Result<(), SystemError> {
    if sig == Signal::INVALID {
        return Err(SystemError::EINVAL);
    }

    let pcb = ProcessManager::current_pcb();
    let ignores = |action: &Sigaction| {
        action.is_ignore() || (action.is_default() && SIG_KERNEL_IGNORE_MASK.contains(sig.into()))
    };
    // Stabilize group publication before changing a disposition that flushes
    // every member. Reserve outside locks; pin all members until after the
    // sighand guard is gone (last PCB drop itself detaches its sighand).
    let mut tasks = Vec::new();
    let membership = if act.as_ref().is_some_and(|action| ignores(action)) {
        Some(loop {
            let guard = crate::process::pid::pid_membership_lock();
            match ProcessManager::fill_thread_group_snapshot(&pcb, &mut tasks) {
                Ok(_) => break guard,
                Err(required) => {
                    drop(guard);
                    tasks
                        .try_reserve(required)
                        .map_err(|_| SystemError::ENOMEM)?;
                }
            }
        })
    } else {
        None
    };
    let sighand = pcb.sighand();
    let mut sighand_guard = sighand.inner_mut();
    // 指向当前信号的action的引用
    let action: &mut Sigaction = &mut sighand_guard.handlers[SigHand::sig2idx(sig)];

    // 对比 MUSL 和 relibc ， 暂时不设置这个标志位
    // if action.flags().contains(SigFlags::SA_FLAG_IMMUTABLE) {
    //     return Err(SystemError::EINVAL);
    // }

    // 保存原有的 sigaction
    let mut old_act: Option<&mut Sigaction> = {
        if let Some(oa) = old_act {
            *(oa) = *action;
            Some(oa)
        } else {
            None
        }
    };
    // 清除所有的脏的sa_flags位（也就是清除那些未使用的）
    let mut act = {
        if let Some(ac) = act {
            *ac.flags_mut() &= SigFlags::SA_ALL;
            Some(ac)
        } else {
            None
        }
    };

    if let Some(act) = &mut old_act {
        *act.flags_mut() &= SigFlags::SA_ALL;
    }

    if let Some(ac) = &mut act {
        // 将act.sa_mask的SIGKILL SIGSTOP的屏蔽清除
        ac.mask_mut()
            .remove(<Signal as Into<SigSet>>::into(Signal::SIGKILL) | Signal::SIGSTOP.into());

        // 将新的sigaction拷贝到进程的action中
        *action = **ac;
        /*
        * 根据POSIX 3.3.1.3规定：
        * 1.不管一个信号是否被阻塞，只要将其设置SIG_IGN，如果当前已经存在了正在pending的信号，那么就把这个信号忽略。
        *
        * 2.不管一个信号是否被阻塞，只要将其设置SIG_DFL，如果当前已经存在了正在pending的信号，
              并且对这个信号的默认处理方式是忽略它，那么就会把pending的信号忽略。
        */
        if ignores(action) {
            let mut mask: SigSet = SigSet::from_bits_truncate(0);
            mask.insert(sig.into());
            pcb.process_signal().shared_pending_flush_by_mask(&mask);
            for task in &tasks {
                task.sig_info_mut().sig_pending_mut().flush_by_mask(&mask);
            }
        }
    }

    drop(sighand_guard);
    drop(membership);
    drop(tasks);
    return Ok(());
}
