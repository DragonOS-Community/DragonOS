use alloc::{
    boxed::Box,
    string::ToString,
    sync::{Arc, Weak},
    vec,
};
use core::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, Ordering},
};
use system_error::SystemError;

use crate::{
    driver::tty::{
        termios::{ControlCharIndex, ControlMode, InputMode, LocalMode, Termios},
        tty_core::{TtyCore, TtyCoreData, TtyFlag, TtyIoctlCmd, TtyPacketStatus},
        tty_device::{TtyDevice, TtyFilePrivateData},
        tty_driver::{
            TtyCorePrivateField, TtyDriver, TtyDriverPrivateData, TtyDriverSubType, TtyOperation,
        },
    },
    exception::workqueue::{Work, WorkQueue},
    filesystem::{
        devpts::DevPtsFs,
        epoll::{event_poll::EventPoll, EPollEventType},
        vfs::{
            file::FileFlags, FilePrivateData, FileSystem, FileType, IndexNode, InodeMode, MountFS,
        },
    },
    libs::{
        casting::DowncastArc,
        mutex::{Mutex, MutexGuard},
        spinlock::SpinLock,
    },
    mm::VirtAddr,
    process::ProcessManager,
    syscall::user_access::UserBufferWriter,
};

lazy_static! {
    static ref PTY_DRAIN_WQ: Arc<WorkQueue> = WorkQueue::new("pty-drain");
}

pub(super) fn pty_drain_workqueue_init() {
    lazy_static::initialize(&PTY_DRAIN_WQ);
}

use super::{ptm_driver, pts_driver, PtyCommon};

pub const NR_UNIX98_PTY_MAX: u32 = 128;
const PTY_BUFFER_LIMIT: usize = 16 * 1024;
const PTY_DRAIN_CHUNK: usize = 256;

fn current_devpts() -> Result<Arc<DevPtsFs>, SystemError> {
    let fs = ProcessManager::current_mntns()
        .root_inode()
        .find("dev")?
        .find("pts")?
        .fs();

    if let Some(devpts) = fs.clone().downcast_arc::<DevPtsFs>() {
        return Ok(devpts);
    }

    fs.downcast_arc::<MountFS>()
        .and_then(|mount_fs| mount_fs.inner_filesystem().downcast_arc::<DevPtsFs>())
        .ok_or(SystemError::ENODEV)
}

#[derive(Debug)]
struct PtyDevPtsLink {
    /// devpts 挂载点根目录（/dev/pts 的 inode），用于精确 unlink 目录项
    pts_root: Weak<dyn IndexNode>,
    /// devpts 文件系统本体，用于精确回收索引（避免再去 downcast/全局路径查找）
    devpts: Weak<DevPtsFs>,
    /// slave 端 inode。TIOCGPTPEER 必须从 master 关联对象打开 peer，
    /// 不能重新按 /dev/pts/N 路径查找，否则目录项被 unlink 后会偏离 Linux 语义。
    slave_inode: Arc<dyn IndexNode>,
    index: usize,
    state: Mutex<PtyDevPtsState>,
    master_to_slave: SpinLock<PtyByteQueue>,
    slave_to_master: SpinLock<PtyByteQueue>,
    master_to_slave_draining: AtomicBool,
    slave_to_master_draining: AtomicBool,
    master_to_slave_drain_requested: AtomicBool,
    slave_to_master_drain_requested: AtomicBool,
    master_to_slave_drain_scheduled: AtomicBool,
    slave_to_master_drain_scheduled: AtomicBool,
    master_to_slave_discarding: AtomicBool,
    slave_to_master_discarding: AtomicBool,
    master_to_slave_flushing: AtomicBool,
    slave_to_master_flushing: AtomicBool,
}

#[derive(Debug, Default)]
struct PtyDevPtsState {
    /// master 侧（ptmx）最后一个 fd 已关闭。
    master_closed: bool,
    /// slave open 已经进入 driver open，但尚未提交为 active fd。
    slave_opening: usize,
    /// 已成功打开的 userspace slave open file description 数量。
    slave_active: usize,
    /// 目录项是否已经 unlink（通常在 master close 时执行）。
    unlinked: bool,
    /// 索引是否已经归还（仅在 master close 且无 opening/active slave 后允许归还）。
    index_freed: bool,
}

#[derive(Debug, Default)]
struct DrainResult {
    delivered: usize,
    freed_backlog: usize,
    still_pending: bool,
    yielded: bool,
}

#[derive(Debug)]
struct PtyByteQueue {
    buf: Box<[u8; PTY_BUFFER_LIMIT]>,
    head: usize,
    len: usize,
    discard_len: usize,
}

impl PtyByteQueue {
    fn new() -> Self {
        Self {
            buf: vec![0; PTY_BUFFER_LIMIT]
                .into_boxed_slice()
                .try_into()
                .unwrap(),
            head: 0,
            len: 0,
            discard_len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn room(&self) -> usize {
        PTY_BUFFER_LIMIT - self.len
    }

    fn clear_count(&mut self) -> usize {
        let cleared = self.len;
        self.head = 0;
        self.len = 0;
        self.discard_len = 0;
        cleared
    }

    fn clear(&mut self) {
        self.clear_count();
    }

    fn push_slice(&mut self, buf: &[u8]) -> usize {
        let accepted = buf.len().min(self.room());
        for (i, c) in buf[..accepted].iter().enumerate() {
            let idx = (self.head + self.len + i) % PTY_BUFFER_LIMIT;
            self.buf[idx] = *c;
        }
        self.len += accepted;
        accepted
    }

    fn copy_front(&self, out: &mut [u8]) -> usize {
        let copied = out.len().min(self.len);
        for (i, slot) in out[..copied].iter_mut().enumerate() {
            *slot = self.buf[(self.head + i) % PTY_BUFFER_LIMIT];
        }
        copied
    }

    fn advance_front(&mut self, count: usize) {
        let count = count.min(self.len);
        self.head = (self.head + count) % PTY_BUFFER_LIMIT;
        self.len -= count;
        self.discard_len = self.discard_len.saturating_sub(count);
        if self.len == 0 {
            self.head = 0;
            self.discard_len = 0;
        }
    }

    fn request_discard_prefix(&mut self) {
        self.discard_len = self.len;
    }

    fn discard_requested_prefix(&mut self) -> usize {
        let discard_len = self.discard_len.min(self.len);
        self.advance_front(discard_len);
        discard_len
    }
}

fn wake_pty_packet_readers(tty: &Arc<TtyCore>) -> Result<(), SystemError> {
    let events = EPollEventType::EPOLLPRI | EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM;
    tty.core().read_wq().wakeup_any(events.bits() as u64);
    EventPoll::wakeup_epoll(tty.core().epitems(), events)?;
    Ok(())
}

impl crate::driver::tty::tty_driver::TtyCorePrivateField for PtyDevPtsLink {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl PtyDevPtsLink {
    fn new(
        pts_root: Weak<dyn IndexNode>,
        devpts: Weak<DevPtsFs>,
        slave_inode: Arc<dyn IndexNode>,
        index: usize,
    ) -> Self {
        Self {
            pts_root,
            devpts,
            slave_inode,
            index,
            state: Mutex::new(PtyDevPtsState::default()),
            master_to_slave: SpinLock::new(PtyByteQueue::new()),
            slave_to_master: SpinLock::new(PtyByteQueue::new()),
            master_to_slave_draining: AtomicBool::new(false),
            slave_to_master_draining: AtomicBool::new(false),
            master_to_slave_drain_requested: AtomicBool::new(false),
            slave_to_master_drain_requested: AtomicBool::new(false),
            master_to_slave_drain_scheduled: AtomicBool::new(false),
            slave_to_master_drain_scheduled: AtomicBool::new(false),
            master_to_slave_discarding: AtomicBool::new(false),
            slave_to_master_discarding: AtomicBool::new(false),
            master_to_slave_flushing: AtomicBool::new(false),
            slave_to_master_flushing: AtomicBool::new(false),
        }
    }

    fn queue_for_source(&self, subtype: TtyDriverSubType) -> Option<&SpinLock<PtyByteQueue>> {
        match subtype {
            TtyDriverSubType::PtyMaster => Some(&self.master_to_slave),
            TtyDriverSubType::PtySlave => Some(&self.slave_to_master),
            _ => None,
        }
    }

    fn drain_scheduled_for_source(&self, subtype: TtyDriverSubType) -> Option<&AtomicBool> {
        match subtype {
            TtyDriverSubType::PtyMaster => Some(&self.master_to_slave_drain_scheduled),
            TtyDriverSubType::PtySlave => Some(&self.slave_to_master_drain_scheduled),
            _ => None,
        }
    }

    fn state_flags_for_source(
        &self,
        subtype: TtyDriverSubType,
    ) -> Option<(&AtomicBool, &AtomicBool, &AtomicBool, &AtomicBool)> {
        match subtype {
            TtyDriverSubType::PtyMaster => Some((
                &self.master_to_slave_draining,
                &self.master_to_slave_drain_requested,
                &self.master_to_slave_discarding,
                &self.master_to_slave_flushing,
            )),
            TtyDriverSubType::PtySlave => Some((
                &self.slave_to_master_draining,
                &self.slave_to_master_drain_requested,
                &self.slave_to_master_discarding,
                &self.slave_to_master_flushing,
            )),
            _ => None,
        }
    }

    fn wake_source_writer(to: &Arc<TtyCore>) {
        if let Some(source) = to.core().link() {
            source.tty_wakeup();
        }
    }

    fn pending_write_room(&self, subtype: TtyDriverSubType) -> usize {
        self.queue_for_source(subtype)
            .map(|queue| queue.lock_irqsave().room())
            .unwrap_or(0)
    }

    fn clear_pending_from(&self, subtype: TtyDriverSubType) -> Result<(), SystemError> {
        let Some(queue) = self.queue_for_source(subtype) else {
            return Err(SystemError::ENODEV);
        };
        let Some((draining, drain_requested, discarding, flushing)) =
            self.state_flags_for_source(subtype)
        else {
            return Err(SystemError::ENODEV);
        };

        while flushing
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }

        while draining
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }

        drain_requested.store(false, Ordering::Release);
        discarding.store(false, Ordering::Release);
        queue.lock_irqsave().clear();
        draining.store(false, Ordering::Release);
        flushing.store(false, Ordering::Release);
        Ok(())
    }

    fn begin_flush_from(&self, subtype: TtyDriverSubType) -> Result<(), SystemError> {
        let Some(queue) = self.queue_for_source(subtype) else {
            return Err(SystemError::ENODEV);
        };
        let Some((draining, drain_requested, discarding, flushing)) =
            self.state_flags_for_source(subtype)
        else {
            return Err(SystemError::ENODEV);
        };

        while flushing
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }

        while draining
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }

        drain_requested.store(false, Ordering::Release);
        discarding.store(false, Ordering::Release);
        queue.lock_irqsave().clear();
        draining.store(false, Ordering::Release);
        Ok(())
    }

    fn finish_flush_from(&self, subtype: TtyDriverSubType) -> Result<(), SystemError> {
        let Some((_, drain_requested, discarding, flushing)) = self.state_flags_for_source(subtype)
        else {
            return Err(SystemError::ENODEV);
        };

        drain_requested.store(false, Ordering::Release);
        discarding.store(false, Ordering::Release);
        flushing.store(false, Ordering::Release);
        Ok(())
    }

    fn request_receive_flush_from(&self, subtype: TtyDriverSubType) -> Result<(), SystemError> {
        let Some(queue) = self.queue_for_source(subtype) else {
            return Err(SystemError::ENODEV);
        };
        let Some((draining, drain_requested, discarding, flushing)) =
            self.state_flags_for_source(subtype)
        else {
            return Err(SystemError::ENODEV);
        };

        while flushing.load(Ordering::Acquire) {
            spin_loop();
        }

        if draining
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            drain_requested.store(false, Ordering::Release);
            discarding.store(false, Ordering::Release);
            queue.lock_irqsave().clear();
            draining.store(false, Ordering::Release);
        } else {
            queue.lock_irqsave().request_discard_prefix();
            drain_requested.store(true, Ordering::Release);
        }

        Ok(())
    }

    fn discard_requested_prefix(&self, queue: &SpinLock<PtyByteQueue>) -> usize {
        queue.lock_irqsave().discard_requested_prefix()
    }

    fn write_to_peer(
        &self,
        owner: Arc<dyn TtyCorePrivateField>,
        subtype: TtyDriverSubType,
        to: Arc<TtyCore>,
        buf: &[u8],
        nr: usize,
    ) -> Result<usize, SystemError> {
        let Some(queue) = self.queue_for_source(subtype) else {
            return Err(SystemError::ENODEV);
        };
        let Some((_, _, discarding, flushing)) = self.state_flags_for_source(subtype) else {
            return Err(SystemError::ENODEV);
        };

        let mut accepted = loop {
            while flushing.load(Ordering::Acquire) || discarding.load(Ordering::Acquire) {
                spin_loop();
            }

            let mut queue_guard = queue.lock_irqsave();
            if flushing.load(Ordering::Acquire) || discarding.load(Ordering::Acquire) {
                drop(queue_guard);
                spin_loop();
                continue;
            }
            break queue_guard.push_slice(&buf[..nr]);
        };

        // Preserve the existing in-order fast path when the peer termios state
        // is stable. If a peer termios writer is active, never block while the
        // source endpoint may hold its own termios/output locks: defer delivery
        // to the workqueue and break the cross-endpoint ABBA dependency.
        let Some(_peer_termios_guard) = to.core().termios_try_read_lock() else {
            Self::schedule_peer_drain(owner, subtype, to);
            return Ok(accepted);
        };

        if accepted == 0 {
            let result = self.drain_to_peer_termios_locked(subtype, to.clone(), usize::MAX)?;
            if result.freed_backlog != 0 {
                Self::wake_source_writer(&to);
            }
            accepted = loop {
                while flushing.load(Ordering::Acquire) || discarding.load(Ordering::Acquire) {
                    spin_loop();
                }

                let mut queue_guard = queue.lock_irqsave();
                if flushing.load(Ordering::Acquire) || discarding.load(Ordering::Acquire) {
                    drop(queue_guard);
                    spin_loop();
                    continue;
                }
                break queue_guard.push_slice(&buf[..nr]);
            };
        }

        if accepted != 0 {
            let result = self.drain_to_peer_termios_locked(subtype, to.clone(), usize::MAX)?;
            if result.freed_backlog != 0 {
                Self::wake_source_writer(&to);
            }
        }
        Ok(accepted)
    }

    fn schedule_peer_drain(
        owner: Arc<dyn TtyCorePrivateField>,
        subtype: TtyDriverSubType,
        to: Arc<TtyCore>,
    ) {
        let Some(hook) = owner.as_any().downcast_ref::<PtyDevPtsLink>() else {
            return;
        };
        let Some(scheduled) = hook.drain_scheduled_for_source(subtype) else {
            return;
        };
        if scheduled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        PTY_DRAIN_WQ.enqueue(Work::new(move || {
            let Some(hook) = owner.as_any().downcast_ref::<PtyDevPtsLink>() else {
                return;
            };
            let Some(scheduled) = hook.drain_scheduled_for_source(subtype) else {
                return;
            };

            let result = hook.drain_to_peer_budgeted(subtype, to.clone(), 16);
            if let Ok(result) = result.as_ref() {
                if result.freed_backlog != 0 {
                    Self::wake_source_writer(&to);
                }
            }

            scheduled.store(false, Ordering::Release);
            let should_retry = result
                .as_ref()
                .map(|result| result.yielded || !result.still_pending)
                .unwrap_or(false)
                && hook
                    .queue_for_source(subtype)
                    .map(|queue| !queue.lock_irqsave().is_empty())
                    .unwrap_or(false);
            if should_retry {
                Self::schedule_peer_drain(owner.clone(), subtype, to.clone());
            }
        }));
    }

    fn drain_to_peer(
        &self,
        subtype: TtyDriverSubType,
        to: Arc<TtyCore>,
    ) -> Result<DrainResult, SystemError> {
        let _termios_guard = to.core().termios_read_lock();
        self.drain_to_peer_termios_locked(subtype, to.clone(), usize::MAX)
    }

    fn drain_to_peer_budgeted(
        &self,
        subtype: TtyDriverSubType,
        to: Arc<TtyCore>,
        max_chunks: usize,
    ) -> Result<DrainResult, SystemError> {
        let _termios_guard = to.core().termios_read_lock();
        self.drain_to_peer_termios_locked(subtype, to.clone(), max_chunks)
    }

    fn drain_to_peer_termios_locked(
        &self,
        subtype: TtyDriverSubType,
        to: Arc<TtyCore>,
        max_chunks: usize,
    ) -> Result<DrainResult, SystemError> {
        let Some(queue) = self.queue_for_source(subtype) else {
            return Err(SystemError::ENODEV);
        };
        let Some((draining, drain_requested, discarding, flushing)) =
            self.state_flags_for_source(subtype)
        else {
            return Err(SystemError::ENODEV);
        };

        let mut result = DrainResult::default();
        if flushing.load(Ordering::Acquire) || discarding.load(Ordering::Acquire) {
            result.still_pending = !queue.lock_irqsave().is_empty();
            return Ok(result);
        }
        if draining
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            drain_requested.store(true, Ordering::Release);
            result.still_pending = !queue.lock_irqsave().is_empty();
            return Ok(result);
        }

        let mut chunks = 0;
        loop {
            if flushing.load(Ordering::Acquire) {
                let _ = self.discard_requested_prefix(queue);
                result.still_pending = !queue.lock_irqsave().is_empty();
                draining.store(false, Ordering::Release);
                break;
            }
            if discarding.load(Ordering::Acquire) {
                result.still_pending = !queue.lock_irqsave().is_empty();
                draining.store(false, Ordering::Release);
                break;
            }

            let discarded = self.discard_requested_prefix(queue);
            if discarded != 0 {
                result.freed_backlog += discarded;
                if !queue.lock_irqsave().is_empty() {
                    continue;
                }
                draining.store(false, Ordering::Release);
                if drain_requested.swap(false, Ordering::AcqRel)
                    && draining
                        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                {
                    continue;
                }
                break;
            }

            let mut chunk = [0u8; PTY_DRAIN_CHUNK];
            let chunk_len = queue.lock_irqsave().copy_front(&mut chunk);

            if chunk_len == 0 {
                draining.store(false, Ordering::Release);
                if drain_requested.swap(false, Ordering::AcqRel)
                    && draining
                        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                {
                    continue;
                }
                break;
            }

            if flushing.load(Ordering::Acquire) {
                queue.lock_irqsave().advance_front(chunk_len);
                result.freed_backlog += chunk_len;
                draining.store(false, Ordering::Release);
                break;
            }
            if discarding.load(Ordering::Acquire) {
                queue.lock_irqsave().request_discard_prefix();
                continue;
            }

            let delivered = match to.core().port().unwrap().receive_buf_termios_locked(
                &chunk[..chunk_len],
                &[],
                chunk_len,
            ) {
                Ok(delivered) => delivered,
                Err(err) => {
                    let _ = self.discard_requested_prefix(queue);
                    draining.store(false, Ordering::Release);
                    return Err(err);
                }
            };
            queue.lock_irqsave().advance_front(delivered);
            result.delivered += delivered;
            result.freed_backlog += delivered;
            let discarded = self.discard_requested_prefix(queue);
            if discarded != 0 {
                result.freed_backlog += discarded;
                if !queue.lock_irqsave().is_empty() {
                    continue;
                }
                draining.store(false, Ordering::Release);
                if drain_requested.swap(false, Ordering::AcqRel)
                    && draining
                        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                {
                    continue;
                }
                break;
            }
            if delivered < chunk_len {
                result.still_pending = true;
                draining.store(false, Ordering::Release);
                break;
            }
            chunks += 1;
            if chunks >= max_chunks && !queue.lock_irqsave().is_empty() {
                result.still_pending = true;
                result.yielded = true;
                draining.store(false, Ordering::Release);
                break;
            }
        }

        if !result.still_pending {
            result.still_pending = !queue.lock_irqsave().is_empty();
        }

        Ok(result)
    }

    fn on_close(&self, subtype: TtyDriverSubType) {
        match subtype {
            TtyDriverSubType::PtyMaster => {
                self.state.lock().master_closed = true;
                // Linux 语义：master 关闭后，/dev/pts/N 目录项应从 devpts 中消失；
                // 但索引不能立即复用（slave 可能仍持有打开的 fd），因此 unlink 与 free_index 分离。
                self.try_unlink_once();
            }
            TtyDriverSubType::PtySlave => {
                // Slave file close is tracked by on_slave_file_close(), because driver close is
                // only reached when the final tty reference is released.
            }
            _ => {}
        }

        self.try_free_index_when_fully_closed();
    }

    fn begin_slave_open(&self) -> Result<(), SystemError> {
        let mut state = self.state.lock();
        if state.master_closed || state.index_freed {
            return Err(SystemError::EIO);
        }
        state.slave_opening += 1;
        Ok(())
    }

    fn finish_slave_open(&self) {
        {
            let mut state = self.state.lock();
            if state.slave_opening == 0 {
                log::warn!(
                    "PtyDevPtsLink: finish slave open without matching begin, index={}",
                    self.index
                );
                return;
            }
            state.slave_opening -= 1;
            state.slave_active += 1;
        }
        self.try_free_index_when_fully_closed();
    }

    fn on_slave_file_close(&self) {
        {
            let mut state = self.state.lock();
            if state.slave_active == 0 {
                log::warn!(
                    "PtyDevPtsLink: slave file close without active open, index={}",
                    self.index
                );
            } else {
                state.slave_active -= 1;
            }
        }
        self.try_free_index_when_fully_closed();
    }

    fn abort_slave_open(&self) {
        {
            let mut state = self.state.lock();
            if state.slave_opening == 0 {
                log::warn!(
                    "PtyDevPtsLink: abort slave open without matching begin, index={}",
                    self.index
                );
                return;
            }
            state.slave_opening -= 1;
        }
        self.try_free_index_when_fully_closed();
    }

    fn try_unlink_once(&self) {
        let should_unlink = {
            let mut state = self.state.lock();
            if state.unlinked {
                false
            } else {
                state.unlinked = true;
                true
            }
        };
        if !should_unlink {
            return;
        }
        if let Some(root) = self.pts_root.upgrade() {
            let _ = root.unlink(&self.index.to_string());
        }
    }

    fn try_free_index_when_fully_closed(&self) {
        let (should_unlink, should_free_index) = {
            let mut state = self.state.lock();
            if !state.master_closed
                || state.slave_opening != 0
                || state.slave_active != 0
                || state.index_freed
            {
                (false, false)
            } else {
                state.index_freed = true;
                let should_unlink = !state.unlinked;
                state.unlinked = true;
                (should_unlink, true)
            }
        };

        if !should_free_index {
            return;
        }

        if should_unlink {
            if let Some(root) = self.pts_root.upgrade() {
                let _ = root.unlink(&self.index.to_string());
            }
        }

        if let Some(devpts) = self.devpts.upgrade() {
            devpts.free_index(self.index);
        }
    }

    fn slave_inode(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        Ok(self.slave_inode.clone())
    }
}

#[derive(Debug)]
pub struct Unix98PtyDriverInner;

impl Unix98PtyDriverInner {
    pub fn new() -> Self {
        Self
    }
}

impl TtyOperation for Unix98PtyDriverInner {
    fn install(&self, driver: Arc<TtyDriver>, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        PtyCommon::pty_common_install(driver, tty, false)
    }

    fn open(&self, tty: &TtyCoreData) -> Result<(), SystemError> {
        let subtype = tty.driver().tty_driver_sub_type();

        if subtype == TtyDriverSubType::PtySlave {
            if let Some(hook_arc) = tty.private_fields() {
                if let Some(hook) = hook_arc.as_any().downcast_ref::<PtyDevPtsLink>() {
                    hook.begin_slave_open()?;
                    return match PtyCommon::pty_common_open(tty) {
                        Ok(()) => {
                            hook.finish_slave_open();
                            Ok(())
                        }
                        Err(err) => {
                            hook.abort_slave_open();
                            Err(err)
                        }
                    };
                }
            }
        }

        PtyCommon::pty_common_open(tty)?;

        Ok(())
    }

    fn write(&self, tty: &TtyCoreData, buf: &[u8], nr: usize) -> Result<usize, SystemError> {
        let to = tty.checked_link()?;

        if nr == 0 || tty.flow_irqsave().stopped {
            return Ok(0);
        }

        if let Some(hook_arc) = tty.private_fields() {
            if let Some(hook) = hook_arc.as_any().downcast_ref::<PtyDevPtsLink>() {
                return hook.write_to_peer(
                    hook_arc.clone(),
                    tty.driver().tty_driver_sub_type(),
                    to,
                    buf,
                    nr,
                );
            }
        }

        to.core().port().unwrap().receive_buf(buf, &[], nr)
    }

    fn write_room(&self, tty: &TtyCoreData) -> usize {
        if tty.flow_irqsave().stopped {
            return 0;
        }

        if let Some(hook_arc) = tty.private_fields() {
            if let Some(hook) = hook_arc.as_any().downcast_ref::<PtyDevPtsLink>() {
                return hook.pending_write_room(tty.driver().tty_driver_sub_type());
            }
        }

        PTY_BUFFER_LIMIT
    }

    fn flush_buffer(&self, tty: &TtyCoreData) -> Result<(), SystemError> {
        let to = tty.link();

        if let Some(hook_arc) = tty.private_fields() {
            if let Some(hook) = hook_arc.as_any().downcast_ref::<PtyDevPtsLink>() {
                hook.clear_pending_from(tty.driver().tty_driver_sub_type())?;
                // clear_pending_from() can satisfy a concurrent drain wait in
                // exactly the same way as normal queue delivery.  Wake the
                // source endpoint after making the zero-backlog state visible.
                if let Some(to) = to.as_ref() {
                    PtyDevPtsLink::wake_source_writer(to);
                } else {
                    tty.write_wq().wakeup_all();
                }
            }
        }

        let Some(to) = to else {
            return Ok(());
        };

        if to.core().contorl_info_irqsave().packet {
            tty.contorl_info_irqsave()
                .pktstatus
                .insert(TtyPacketStatus::TIOCPKT_FLUSHWRITE);
            let _ = wake_pty_packet_readers(&to);
        }

        to.core().read_wq().wakeup_all();

        Ok(())
    }

    fn ioctl(&self, tty: Arc<TtyCore>, cmd: u32, arg: usize) -> Result<(), SystemError> {
        let core = tty.core();
        if core.driver().tty_driver_sub_type() != TtyDriverSubType::PtyMaster {
            // TODO:implement other ioctl commands
            // log::warn!("Unix98PtyDriver: ioctl called on non-pty master: {cmd:#x}");
            return Err(SystemError::ENOIOCTLCMD);
        }
        match cmd {
            TtyIoctlCmd::TIOCSPTLCK => {
                return PtyCommon::pty_set_lock(core, VirtAddr::new(arg));
            }
            TtyIoctlCmd::TIOCGPTLCK => {
                return PtyCommon::pty_get_lock(core, VirtAddr::new(arg));
            }
            TtyIoctlCmd::TIOCPKT => {
                return PtyCommon::pty_set_packet_mode(core, VirtAddr::new(arg));
            }
            TtyIoctlCmd::TIOCGPKT => {
                return PtyCommon::pty_get_packet_mode(core, VirtAddr::new(arg));
            }
            TtyIoctlCmd::TIOCGPTN => {
                let mut user_writer =
                    UserBufferWriter::new(arg as *mut u32, core::mem::size_of::<u32>(), true)?;

                return user_writer.copy_one_to_user(&(core.index() as u32), 0);
            }
            _ => {
                // TODO: implement other ioctl commands
                // log::error!("Unix98PtyDriver: Unsupported ioctl cmd: {cmd:#x}");
                return Err(SystemError::ENOIOCTLCMD);
            }
        }
    }

    fn set_termios(&self, tty: Arc<TtyCore>, old_termios: Termios) -> Result<(), SystemError> {
        let core = tty.core();
        if core.driver().tty_driver_sub_type() != TtyDriverSubType::PtySlave {
            return Err(SystemError::ENOSYS);
        }

        let core = tty.core();
        if let Some(link) = core.link() {
            let link = link.core();
            if link.contorl_info_irqsave().packet {
                let curr_termios = *core.termios();
                let extproc = old_termios.local_mode.contains(LocalMode::EXTPROC)
                    | curr_termios.local_mode.contains(LocalMode::EXTPROC);

                let old_flow = old_termios.input_mode.contains(InputMode::IXON)
                    && old_termios.control_characters[ControlCharIndex::VSTOP] == 0o023
                    && old_termios.control_characters[ControlCharIndex::VSTART] == 0o021;

                let new_flow = curr_termios.input_mode.contains(InputMode::IXON)
                    && curr_termios.control_characters[ControlCharIndex::VSTOP] == 0o023
                    && curr_termios.control_characters[ControlCharIndex::VSTART] == 0o021;

                if old_flow != new_flow || extproc {
                    let mut ctrl = core.contorl_info_irqsave();
                    if old_flow != new_flow {
                        ctrl.pktstatus.remove(
                            TtyPacketStatus::TIOCPKT_DOSTOP | TtyPacketStatus::TIOCPKT_NOSTOP,
                        );

                        if new_flow {
                            ctrl.pktstatus.insert(TtyPacketStatus::TIOCPKT_DOSTOP);
                        } else {
                            ctrl.pktstatus.insert(TtyPacketStatus::TIOCPKT_NOSTOP);
                        }
                    }

                    if extproc {
                        ctrl.pktstatus.insert(TtyPacketStatus::TIOCPKT_IOCTL);
                    }

                    let events = EPollEventType::EPOLLPRI
                        | EPollEventType::EPOLLIN
                        | EPollEventType::EPOLLRDNORM;
                    link.read_wq().wakeup_all();
                    let _ = EventPoll::wakeup_epoll(link.epitems(), events);
                }
            }
        }
        let mut termois = core.termios_write();
        termois
            .control_mode
            .remove(ControlMode::CSIZE | ControlMode::PARENB);
        termois
            .control_mode
            .insert(ControlMode::CS8 | ControlMode::CREAD);
        Ok(())
    }

    fn start(&self, core: &TtyCoreData) -> Result<(), SystemError> {
        if core.driver().tty_driver_sub_type() != TtyDriverSubType::PtySlave {
            return Err(SystemError::ENOSYS);
        }

        let link = core.checked_link()?;

        let mut ctrl = core.contorl_info_irqsave();
        ctrl.pktstatus.remove(TtyPacketStatus::TIOCPKT_STOP);
        ctrl.pktstatus.insert(TtyPacketStatus::TIOCPKT_START);

        let events =
            EPollEventType::EPOLLPRI | EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM;
        link.core().read_wq().wakeup_any(events.bits() as u64);
        let _ = EventPoll::wakeup_epoll(link.core().epitems(), events);

        Ok(())
    }

    fn stop(&self, core: &TtyCoreData) -> Result<(), SystemError> {
        if core.driver().tty_driver_sub_type() != TtyDriverSubType::PtySlave {
            return Err(SystemError::ENOSYS);
        }

        let link = core.checked_link()?;

        let mut ctrl = core.contorl_info_irqsave();
        ctrl.pktstatus.remove(TtyPacketStatus::TIOCPKT_START);
        ctrl.pktstatus.insert(TtyPacketStatus::TIOCPKT_STOP);

        let events =
            EPollEventType::EPOLLPRI | EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM;
        link.core().read_wq().wakeup_any(events.bits() as u64);
        let _ = EventPoll::wakeup_epoll(link.core().epitems(), events);

        Ok(())
    }

    fn flush_chars(&self, _tty: &TtyCoreData) {
        // Linux pty does not implement flush_chars; writes are pushed by the
        // pty write path itself rather than by recursively draining here.
    }

    fn lookup(
        &self,
        index: usize,
        priv_data: TtyDriverPrivateData,
    ) -> Result<Arc<TtyCore>, SystemError> {
        if let TtyDriverPrivateData::Pty(false) = priv_data {
            // Unix98 slave ttys are created only by ptmx. A missing peer must fail like Linux
            // pts_unix98_lookup() instead of letting the generic path construct a new tty.
            return pts_driver()
                .ttys()
                .get(&index)
                .cloned()
                .ok_or(SystemError::EIO);
        }

        return Err(SystemError::ENOSYS);
    }

    fn close(&self, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        let driver = tty.core().driver();
        let core = tty.core();
        let subtype = driver.tty_driver_sub_type();

        core.flags_write().insert(TtyFlag::IO_ERROR);
        core.read_wq().wakeup_all();
        core.write_wq().wakeup_all();
        core.contorl_info_irqsave().packet = false;

        if subtype == TtyDriverSubType::PtySlave {
            let mut peer_closed = true;
            if let Some(link) = core.link() {
                let link_core = link.core();
                peer_closed = link_core.flags().contains(TtyFlag::IO_ERROR);
                // set OTHER_CLOSED flag to tell master side that the slave side is closed
                link_core.flags_write().insert(TtyFlag::OTHER_CLOSED);
                // wake up waiting read/write queues on master side
                link_core.read_wq().wakeup_all();
                link_core.write_wq().wakeup_all();
                // wake up epoll events
                let epitems = link_core.epitems();
                let _ = EventPoll::wakeup_epoll(epitems, EPollEventType::EPOLLHUP);
            }
            if peer_closed {
                driver.ttys().remove(&core.index());
            }
        } else if subtype == TtyDriverSubType::PtyMaster {
            // master 侧最后关闭：从 driver 表移除自身（避免泄漏）；devpts 的释放由 hook 统一处理
            driver.ttys().remove(&core.index());
            core.flags_write().insert(TtyFlag::OTHER_CLOSED);
            if let Some(link) = core.link() {
                let link_core = link.core();
                link_core.flags_write().insert(TtyFlag::OTHER_CLOSED);
                link_core.read_wq().wakeup_all();
                link_core.write_wq().wakeup_all();
                let epitems = link_core.epitems();
                let _ = EventPoll::wakeup_epoll(epitems, EPollEventType::EPOLLHUP);
                TtyCore::tty_vhangup(link.clone());
                if link_core.flags().contains(TtyFlag::IO_ERROR) {
                    link_core.driver().ttys().remove(&link_core.index());
                }
            }
        }

        // 通过 hook 精确管理 devpts 目录项与索引生命周期。必须放在 driver 表
        // 解绑之后，避免 index 释放后被新 PTY 复用又被旧 close 删除。
        if let Some(hook_arc) = tty.private_fields() {
            if let Some(hook) = hook_arc.as_any().downcast_ref::<PtyDevPtsLink>() {
                hook.on_close(subtype);
            }
        }

        Ok(())
    }

    fn resize(
        &self,
        tty: Arc<TtyCore>,
        winsize: crate::driver::tty::termios::WindowSize,
    ) -> Result<(), SystemError> {
        let core = tty.core();
        if *core.window_size() == winsize {
            return Ok(());
        }

        // TODO：向进程发送SIGWINCH信号

        *core.window_size_write() = winsize;
        *core.link().unwrap().core().window_size_write() = winsize;

        Ok(())
    }
}

pub fn ptmx_open(
    this: &TtyDevice,
    mut data: MutexGuard<FilePrivateData>,
    flags: &FileFlags,
) -> Result<(), SystemError> {
    if let FilePrivateData::Tty(data) = &*data {
        let tty = data.tty();
        // log::debug!("ptmx_open: already opened :{:p}, tty core: {:?}", tty, tty.core().name());
        tty.core().add_count();
        return Ok(());
    }
    // 根据当前节点所属的文件系统决定 devpts 根
    let fsinfo = if let Some(devpts) = this.fs().clone().downcast_arc::<DevPtsFs>() {
        devpts
    } else {
        current_devpts()?
    };
    let pts_root_inode = fsinfo.root_inode();

    let index = fsinfo.alloc_index()?;

    let tty = match ptm_driver().init_tty_device(Some(index)) {
        Ok(tty) => tty,
        Err(err) => {
            fsinfo.free_index(index);
            return Err(err);
        }
    };

    // 设置privdata
    *data = FilePrivateData::Tty(TtyFilePrivateData {
        tty: tty.clone(),
        flags: *flags,
    });

    let core = tty.core();
    core.flags_write().insert(TtyFlag::PTY_LOCK);

    let slave_inode = match pts_root_inode.create(
        &index.to_string(),
        FileType::CharDevice,
        InodeMode::from_bits_truncate(0x666),
    ) {
        Ok(slave_inode) => slave_inode,
        Err(err) => {
            ptm_driver().ttys().remove(&index);
            pts_driver().ttys().remove(&index);
            fsinfo.free_index(index);
            *data = FilePrivateData::Unused;
            return Err(err);
        }
    };

    // 在 master/slave 两端记录 devpts 根目录与 fs，用于精确清理：
    // - master close: unlink /dev/pts/N
    // - master+slave 都 close: free_index(N)
    let hook = Arc::new(PtyDevPtsLink::new(
        Arc::downgrade(&pts_root_inode),
        Arc::downgrade(&fsinfo),
        slave_inode,
        index,
    ));
    tty.set_private_fields(hook.clone());
    if let Some(slave) = tty.core().link() {
        slave.set_private_fields(hook);
    }

    if let Err(err) = ptm_driver().driver_funcs().open(core) {
        ptm_driver().ttys().remove(&index);
        pts_driver().ttys().remove(&index);
        let _ = pts_root_inode.unlink(&index.to_string());
        fsinfo.free_index(index);
        *data = FilePrivateData::Unused;
        return Err(err);
    }

    Ok(())
}

pub fn ptm_peer_inode(master: Arc<TtyCore>) -> Result<Arc<dyn IndexNode>, SystemError> {
    let core = master.core();
    if core.driver().tty_driver_sub_type() != TtyDriverSubType::PtyMaster {
        return Err(SystemError::EIO);
    }

    let hook_arc = master.private_fields().ok_or(SystemError::EIO)?;
    let hook = hook_arc
        .as_any()
        .downcast_ref::<PtyDevPtsLink>()
        .ok_or(SystemError::EIO)?;
    hook.slave_inode()
}

pub fn pty_file_close(tty: &TtyCore) {
    let core = tty.core();
    if core.driver().tty_driver_sub_type() != TtyDriverSubType::PtySlave {
        return;
    }

    if let Some(hook_arc) = tty.private_fields() {
        if let Some(hook) = hook_arc.as_any().downcast_ref::<PtyDevPtsLink>() {
            hook.on_slave_file_close();
        }
    }
}

pub fn pty_drain_pending_to(tty: Arc<TtyCore>) -> Result<(), SystemError> {
    let Some(peer) = tty.core().link() else {
        return Ok(());
    };
    let Some(hook_arc) = tty.private_fields() else {
        return Ok(());
    };
    let Some(hook) = hook_arc.as_any().downcast_ref::<PtyDevPtsLink>() else {
        return Ok(());
    };

    let result = hook.drain_to_peer(peer.core().driver().tty_driver_sub_type(), tty)?;
    if result.freed_backlog != 0 {
        peer.tty_wakeup();
    }
    Ok(())
}

pub fn pty_flush_input_buffer<F>(tty: Arc<TtyCore>, clear_input: F) -> Result<(), SystemError>
where
    F: FnOnce(),
{
    let Some(peer) = tty.core().link() else {
        clear_input();
        return Ok(());
    };
    let Some(hook_arc) = tty.private_fields() else {
        clear_input();
        return Ok(());
    };
    let Some(hook) = hook_arc.as_any().downcast_ref::<PtyDevPtsLink>() else {
        clear_input();
        return Ok(());
    };

    let subtype = peer.core().driver().tty_driver_sub_type();
    hook.begin_flush_from(subtype)?;
    clear_input();
    hook.finish_flush_from(subtype)?;
    peer.tty_wakeup();
    Ok(())
}

pub fn pty_receive_flush_input_buffer<F>(
    tty: Arc<TtyCore>,
    clear_input: F,
) -> Result<(), SystemError>
where
    F: FnOnce(),
{
    let Some(peer) = tty.core().link() else {
        clear_input();
        return Ok(());
    };
    let Some(hook_arc) = tty.private_fields() else {
        clear_input();
        return Ok(());
    };
    let Some(hook) = hook_arc.as_any().downcast_ref::<PtyDevPtsLink>() else {
        clear_input();
        return Ok(());
    };

    let subtype = peer.core().driver().tty_driver_sub_type();
    hook.request_receive_flush_from(subtype)?;
    clear_input();
    peer.tty_wakeup();
    Ok(())
}
