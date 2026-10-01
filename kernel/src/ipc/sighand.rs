use crate::{
    arch::ipc::signal::{SigFlags, SigSet, Signal, MAX_SIG_NUM},
    filesystem::epoll::event_poll::EPollItemList,
    ipc::signal_types::{SaHandlerType, Sigaction, SigactionType},
    libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
    libs::wait_queue::WaitQueue,
    process::{ProcessControlBlock, ProcessManager},
};
use alloc::{sync::Arc, vec::Vec};
use core::{fmt::Debug, sync::atomic::compiler_fence};
use system_error::SystemError;

/// Signal dispositions may be shared by distinct processes using CLONE_SIGHAND.
/// Process-directed signals and lifecycle state live in ProcessSignalState.
pub struct SigHand {
    inner: RwLock<InnerSigHand>,
    signalfd_wqh: WaitQueue,
    signalfd_epitems: EPollItemList,
}

impl Debug for SigHand {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SigHand").finish()
    }
}

pub struct InnerSigHand {
    pub handlers: Vec<Sigaction>,
    pub cnt: i64,
}

impl SigHand {
    pub fn new() -> Arc<Self> {
        Self::try_new().expect("failed to allocate signal-handler table")
    }
    pub fn try_new() -> Result<Arc<Self>, SystemError> {
        Arc::try_new(Self {
            inner: RwLock::new(InnerSigHand {
                handlers: try_default_sighandlers()?,
                cnt: 0,
            }),
            signalfd_wqh: WaitQueue::default(),
            signalfd_epitems: EPollItemList::default(),
        })
        .map_err(|_| SystemError::ENOMEM)
    }
    fn inner(&self) -> RwLockReadGuard<'_, InnerSigHand> {
        self.inner.read_irqsave()
    }
    fn inner_mut(&self) -> RwLockWriteGuard<'_, InnerSigHand> {
        self.inner.write_irqsave()
    }
    pub fn inner_read(&self) -> RwLockReadGuard<'_, InnerSigHand> {
        self.inner()
    }
    pub fn signalfd_wqh(&self) -> &WaitQueue {
        &self.signalfd_wqh
    }
    pub fn signalfd_epitems(&self) -> &EPollItemList {
        &self.signalfd_epitems
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
        self.inner_mut().handlers = default_sighandlers();
    }
    pub fn handler(&self, sig: Signal) -> Option<Sigaction> {
        self.inner().handlers.get(Self::sig2idx(sig)).cloned()
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

fn default_sighandlers() -> Vec<Sigaction> {
    try_default_sighandlers().expect("failed to allocate signal actions")
}

fn try_default_sighandlers() -> Result<Vec<Sigaction>, SystemError> {
    let mut r = Vec::new();
    r.try_reserve_exact(MAX_SIG_NUM)
        .map_err(|_| SystemError::ENOMEM)?;
    r.resize(MAX_SIG_NUM, Sigaction::default());
    let mut sig_ign = Sigaction::default();
    // 收到忽略的信号，重启系统调用
    // Linux ignores SIGURG/SIGWINCH by default; SIGCHLD is also ignored by default,
    // but the handler must remain SIG_DFL to distinguish default ignore from explicit SIG_IGN.
    sig_ign.set_action(SigactionType::SaHandler(SaHandlerType::Ignore));
    sig_ign.flags_mut().insert(SigFlags::SA_RESTART);

    r[Signal::SIGURG as usize - 1] = sig_ign;
    r[Signal::SIGWINCH as usize - 1] = sig_ign;

    Ok(r)
}

impl ProcessControlBlock {
    /// 刷新指定进程的sighand的sigaction，将满足条件的sigaction恢复为默认状态。
    /// 除非某个信号被设置为忽略且 `force_default` 为 `false`，否则都不会将其恢复。
    ///
    /// # 参数
    ///
    /// - `pcb`: 要被刷新的pcb。
    /// - `force_default`: 是否强制将sigaction恢复成默认状态。
    pub fn flush_signal_handlers(&self, force_default: bool) {
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // debug!("hand=0x{:018x}", hand as *const sighand_struct as usize);
        let sighand = self.sighand();
        let actions = &mut sighand.inner_mut().handlers;

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

pub(super) fn do_sigaction(
    sig: Signal,
    act: Option<&mut Sigaction>,
    old_act: Option<&mut Sigaction>,
) -> Result<(), SystemError> {
    if sig == Signal::INVALID {
        return Err(SystemError::EINVAL);
    }

    let pcb = ProcessManager::current_pcb();
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
        if action.is_ignore() {
            let mut mask: SigSet = SigSet::from_bits_truncate(0);
            mask.insert(sig.into());
            pcb.sig_info_mut().sig_pending_mut().flush_by_mask(&mask);
            // todo: 当有了多个线程后，在这里进行操作，把每个线程的sigqueue都进行刷新
        }
    }

    return Ok(());
}
