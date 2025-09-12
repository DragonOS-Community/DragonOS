use core::{fmt::Debug, sync::atomic::compiler_fence};

use alloc::sync::Arc;

use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::ipc::signal::{SigFlags, SigSet, Signal, MAX_SIG_NUM},
    ipc::signal_types::{SaHandlerType, SigInfo, SigPending, SigactionType, SignalFlags},
    libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
    process::{
        pid::{Pid, PidType},
        ProcessControlBlock, ProcessManager,
    },
};

use super::signal_types::Sigaction;

pub struct SigHand {
    inner: RwLock<InnerSigHand>,
}

impl Debug for SigHand {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SigHand").finish()
    }
}

pub struct InnerSigHand {
    pub handlers: Vec<Sigaction>,
    /// 当前线程所属进程要处理的信号
    pub shared_pending: SigPending,
    pub flags: SignalFlags,
    pub pids: [Option<Arc<Pid>>; PidType::PIDTYPE_MAX],
    /// 在 sighand 上维护的引用计数（与 Linux 一致的布局位置）
    pub cnt: i64,
}

impl SigHand {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(InnerSigHand::default()),
        })
    }

    fn inner(&self) -> RwLockReadGuard<'_, InnerSigHand> {
        self.inner.read_irqsave()
    }

    fn inner_mut(&self) -> RwLockWriteGuard<'_, InnerSigHand> {
        self.inner.write_irqsave()
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
        self_guard.handlers = other_guard.handlers.clone();
    }

    // ===== Shared pending helpers =====
    pub fn shared_pending_signal(&self) -> SigSet {
        let g = self.inner();
        g.shared_pending.signal()
    }

    pub fn shared_pending_flush_by_mask(&self, mask: &SigSet) {
        let mut g = self.inner_mut();
        g.shared_pending.flush_by_mask(mask);
    }

    pub fn shared_pending_queue_has(&self, sig: Signal) -> bool {
        let g = self.inner();
        g.shared_pending.queue().find(sig).0.is_some()
    }

    pub fn shared_pending_dequeue(&self, sig_mask: &SigSet) -> (Signal, Option<SigInfo>) {
        let mut g = self.inner_mut();
        g.shared_pending.dequeue_signal(sig_mask)
    }

    // ===== Signal flags helpers =====
    pub fn flags(&self) -> SignalFlags {
        self.inner().flags
    }

    pub fn flags_contains(&self, flag: SignalFlags) -> bool {
        self.inner().flags.contains(flag)
    }

    pub fn flags_insert(&self, flag: SignalFlags) {
        let mut g = self.inner_mut();
        g.flags.insert(flag);
    }

    pub fn flags_remove(&self, flag: SignalFlags) {
        let mut g = self.inner_mut();
        g.flags.remove(flag);
    }

    // ===== PIDs helpers =====
    pub fn pid(&self, ty: PidType) -> Option<Arc<Pid>> {
        self.inner().pids[ty as usize].clone()
    }

    pub fn set_pid(&self, ty: PidType, pid: Option<Arc<Pid>>) {
        let mut g = self.inner_mut();
        g.pids[ty as usize] = pid;
    }

    // ===== Refcount helpers =====
    pub fn load_count(&self) -> i64 {
        self.inner().cnt
    }
}

impl Default for InnerSigHand {
    fn default() -> Self {
        Self {
            handlers: default_sighandlers(),
            pids: core::array::from_fn(|_| None),
            shared_pending: SigPending::default(),
            flags: SignalFlags::empty(),
            cnt: 0,
        }
    }
}

fn default_sighandlers() -> Vec<Sigaction> {
    let mut r = vec![Sigaction::default(); MAX_SIG_NUM];
    let mut sig_ign = Sigaction::default();
    // 收到忽略的信号，重启系统调用
    // todo: 看看linux哪些
    sig_ign.flags_mut().insert(SigFlags::SA_RESTART);

    r[Signal::SIGCHLD as usize - 1] = sig_ign;
    r[Signal::SIGURG as usize - 1] = sig_ign;
    r[Signal::SIGWINCH as usize - 1] = sig_ign;

    r
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
            sigaction.mask_mut().remove(SigSet::all());
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
