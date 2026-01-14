use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use core::sync::atomic::{AtomicBool, Ordering};
use system_error::SystemError;

use crate::{
    driver::tty::{
        termios::{ControlCharIndex, ControlMode, InputMode, LocalMode, Termios},
        tty_core::{TtyCore, TtyCoreData, TtyFlag, TtyIoctlCmd, TtyPacketStatus},
        tty_device::{TtyDevice, TtyFilePrivateData},
        tty_driver::{TtyDriver, TtyDriverPrivateData, TtyDriverSubType, TtyOperation},
    },
    filesystem::{
        devpts::DevPtsFs,
        epoll::{event_poll::EventPoll, EPollEventType},
        vfs::{file::FileFlags, FilePrivateData, FileSystem, FileType, IndexNode, InodeMode},
    },
    libs::{casting::DowncastArc, mutex::MutexGuard},
    mm::VirtAddr,
    syscall::user_access::UserBufferWriter,
};

use super::{ptm_driver, pts_driver, PtyCommon};

pub const NR_UNIX98_PTY_MAX: u32 = 128;

#[derive(Debug)]
struct PtyDevPtsLink {
    /// devpts 挂载点根目录（/dev/pts 的 inode），用于精确 unlink 目录项
    pts_root: Weak<dyn IndexNode>,
    /// devpts 文件系统本体，用于精确回收索引（避免再去 downcast/全局路径查找）
    devpts: Weak<DevPtsFs>,
    index: usize,
    /// master 侧（ptmx）最后一个 fd 已关闭
    master_closed: AtomicBool,
    /// slave 侧（/dev/pts/N）最后一个 fd 已关闭
    slave_closed: AtomicBool,
    /// 目录项是否已经 unlink（通常在 master close 时执行）
    unlinked: AtomicBool,
    /// 索引是否已经归还（仅在 master+slave 都关闭后才允许归还）
    index_freed: AtomicBool,
}

impl crate::driver::tty::tty_driver::TtyCorePrivateField for PtyDevPtsLink {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl PtyDevPtsLink {
    fn new(pts_root: Weak<dyn IndexNode>, devpts: Weak<DevPtsFs>, index: usize) -> Self {
        Self {
            pts_root,
            devpts,
            index,
            master_closed: AtomicBool::new(false),
            slave_closed: AtomicBool::new(false),
            unlinked: AtomicBool::new(false),
            index_freed: AtomicBool::new(false),
        }
    }

    fn on_close(&self, subtype: TtyDriverSubType) {
        match subtype {
            TtyDriverSubType::PtyMaster => {
                self.master_closed.store(true, Ordering::SeqCst);
                // Linux 语义：master 关闭后，/dev/pts/N 目录项应从 devpts 中消失；
                // 但索引不能立即复用（slave 可能仍持有打开的 fd），因此 unlink 与 free_index 分离。
                self.try_unlink_once();
            }
            TtyDriverSubType::PtySlave => {
                self.slave_closed.store(true, Ordering::SeqCst);
            }
            _ => {}
        }

        self.try_free_index_when_fully_closed();
    }

    fn try_unlink_once(&self) {
        if self.unlinked.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(root) = self.pts_root.upgrade() {
            let _ = root.unlink(&self.index.to_string());
        }
    }

    fn try_free_index_when_fully_closed(&self) {
        if !(self.master_closed.load(Ordering::SeqCst) && self.slave_closed.load(Ordering::SeqCst))
        {
            return;
        }
        if self.index_freed.swap(true, Ordering::SeqCst) {
            return;
        }

        // 兜底：如果 master 未触发 unlink（异常路径），在最终回收时再尝试一次。
        self.try_unlink_once();
        if let Some(devpts) = self.devpts.upgrade() {
            devpts.free_index(self.index);
        }
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
        PtyCommon::pty_common_open(tty)
    }

    fn write(&self, tty: &TtyCoreData, buf: &[u8], nr: usize) -> Result<usize, SystemError> {
        let to = tty.checked_link()?;

        if nr == 0 || tty.flow_irqsave().stopped {
            return Ok(0);
        }

        to.core().port().unwrap().receive_buf(buf, &[], nr)
    }

    fn write_room(&self, tty: &TtyCoreData) -> usize {
        // TODO 暂时
        if tty.flow_irqsave().stopped {
            return 0;
        }

        8192
    }

    fn flush_buffer(&self, tty: &TtyCoreData) -> Result<(), SystemError> {
        let to = tty.checked_link()?;

        let mut ctrl = to.core().contorl_info_irqsave();
        ctrl.pktstatus.insert(TtyPacketStatus::TIOCPKT_FLUSHWRITE);

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

                    link.read_wq().wakeup_all();
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

        link.core()
            .read_wq()
            .wakeup_any(EPollEventType::EPOLLIN.bits() as u64);

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

        link.core()
            .read_wq()
            .wakeup_any(EPollEventType::EPOLLIN.bits() as u64);

        Ok(())
    }

    fn flush_chars(&self, _tty: &TtyCoreData) {
        // 不做处理
    }

    fn lookup(
        &self,
        index: usize,
        priv_data: TtyDriverPrivateData,
    ) -> Result<Arc<TtyCore>, SystemError> {
        if let TtyDriverPrivateData::Pty(false) = priv_data {
            return pts_driver()
                .ttys()
                .get(&index)
                .cloned()
                .ok_or(SystemError::ENODEV);
        }

        return Err(SystemError::ENOSYS);
    }

    fn close(&self, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        let driver = tty.core().driver();
        // 通过 hook 精确管理 devpts 目录项与索引生命周期
        if let Some(hook_arc) = tty.private_fields() {
            if let Some(hook) = hook_arc.as_any().downcast_ref::<PtyDevPtsLink>() {
                hook.on_close(driver.tty_driver_sub_type());
            }
        }

        if driver.tty_driver_sub_type() == TtyDriverSubType::PtySlave {
            driver.ttys().remove(&tty.core().index());
            if let Some(link) = tty.core().link() {
                let link_core = link.core();
                // set OTHER_CLOSED flag to tell master side that the slave side is closed
                link_core.flags_write().insert(TtyFlag::OTHER_CLOSED);
                // wake up waiting read/write queues on master side
                link_core.read_wq().wakeup_all();
                link_core.write_wq().wakeup_all();
                // wake up epoll events
                let epitems = link_core.epitems();
                let _ = EventPoll::wakeup_epoll(epitems, EPollEventType::EPOLLHUP);
            }
        } else if driver.tty_driver_sub_type() == TtyDriverSubType::PtyMaster {
            // master 侧最后关闭：从 driver 表移除自身（避免泄漏）；devpts 的释放由 hook 统一处理
            driver.ttys().remove(&tty.core().index());
            if let Some(link) = tty.core().link() {
                let link_core = link.core();
                link_core.flags_write().insert(TtyFlag::OTHER_CLOSED);
                link_core.read_wq().wakeup_all();
                link_core.write_wq().wakeup_all();
                let epitems = link_core.epitems();
                let _ = EventPoll::wakeup_epoll(epitems, EPollEventType::EPOLLHUP);
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
    let (pts_root_inode, fsinfo) =
        if let Some(devpts) = this.fs().clone().downcast_arc::<DevPtsFs>() {
            let root_inode = devpts.root_inode();
            (root_inode, devpts)
        } else {
            return Err(SystemError::ENODEV);
        };

    let index = fsinfo.alloc_index()?;

    let tty = ptm_driver().init_tty_device(Some(index))?;

    // 设置privdata
    *data = FilePrivateData::Tty(TtyFilePrivateData {
        tty: tty.clone(),
        flags: *flags,
    });

    let core = tty.core();
    core.flags_write().insert(TtyFlag::PTY_LOCK);

    let _ = pts_root_inode.create(
        &index.to_string(),
        FileType::CharDevice,
        InodeMode::from_bits_truncate(0x666),
    )?;

    // 在 master/slave 两端记录 devpts 根目录与 fs，用于精确清理：
    // - master close: unlink /dev/pts/N
    // - master+slave 都 close: free_index(N)
    let hook = Arc::new(PtyDevPtsLink::new(
        Arc::downgrade(&pts_root_inode),
        Arc::downgrade(&fsinfo),
        index,
    ));
    tty.set_private_fields(hook.clone());
    if let Some(slave) = tty.core().link() {
        slave.set_private_fields(hook);
    }

    ptm_driver().driver_funcs().open(core)?;

    Ok(())
}
