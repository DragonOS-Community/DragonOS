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
        vfs::{
            file::FileFlags, mount::MountFSInode, FilePrivateData, FileSystem, FileType, IndexNode,
            InodeMode, MountFS, VFS_MAX_FOLLOW_SYMLINK_TIMES,
        },
    },
    libs::{casting::DowncastArc, spinlock::SpinLockGuard},
    mm::VirtAddr,
    process::ProcessManager,
    syscall::user_access::UserBufferWriter,
};

use super::{ptm_driver, pts_driver, PtyCommon};

pub const NR_UNIX98_PTY_MAX: u32 = 128;

#[derive(Debug)]
struct PtyDevPtsLink {
    pts_root: Weak<MountFSInode>,
    index: usize,
    freed: core::sync::atomic::AtomicBool,
}

impl crate::driver::tty::tty_driver::TtyCorePrivateField for PtyDevPtsLink {
    fn as_any(&self) -> &dyn core::any::Any {
        self
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
        let mut removed = false;

        // 优先通过 hook 删除 devpts 入口并回收索引
        if let Some(hook_arc) = tty.private_fields() {
            if let Some(hook) = hook_arc.as_any().downcast_ref::<PtyDevPtsLink>() {
                // 防止重复释放
                if !hook.freed.swap(true, Ordering::SeqCst) {
                    if let Some(root) = hook.pts_root.upgrade() {
                        let _ = root.unlink(&hook.index.to_string());
                        if let Some(mfs) = root.fs().clone().downcast_arc::<MountFS>() {
                            if let Some(devpts) =
                                mfs.inner_filesystem().clone().downcast_arc::<DevPtsFs>()
                            {
                                devpts.free_index(hook.index);
                            }
                        }
                        removed = true;
                    }
                }
            }
        }

        if driver.tty_driver_sub_type() == TtyDriverSubType::PtySlave {
            driver.ttys().remove(&tty.core().index());
            // 兜底：如果未通过 hook 删除，则直接尝试从 /dev/pts 移除
            if !removed {
                let root_inode = ProcessManager::current_mntns().root_inode();
                if let Ok(pts_root_inode) =
                    root_inode.lookup_follow_symlink("/dev/pts", VFS_MAX_FOLLOW_SYMLINK_TIMES)
                {
                    let _ = pts_root_inode.unlink(&tty.core().index().to_string());
                }
            }
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
            // 主端关闭：尝试删除 devpts 入口；若没有 hook 也兜底一次
            if !removed {
                if let Some(hook_arc) = tty.private_fields() {
                    if let Some(hook) = hook_arc.as_any().downcast_ref::<PtyDevPtsLink>() {
                        if !hook.freed.swap(true, Ordering::SeqCst) {
                            if let Some(root) = hook.pts_root.upgrade() {
                                let _ = root.unlink(&hook.index.to_string());
                                if let Some(mfs) = root.fs().clone().downcast_arc::<MountFS>() {
                                    if let Some(devpts) =
                                        mfs.inner_filesystem().clone().downcast_arc::<DevPtsFs>()
                                    {
                                        devpts.free_index(hook.index);
                                    }
                                }
                                removed = true;
                            }
                        }
                    }
                }
            }
            if !removed {
                let root_inode = ProcessManager::current_mntns().root_inode();
                if let Ok(pts_root_inode) =
                    root_inode.lookup_follow_symlink("/dev/pts", VFS_MAX_FOLLOW_SYMLINK_TIMES)
                {
                    let _ = pts_root_inode.unlink(&tty.core().index().to_string());
                }
            }
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
    mut data: SpinLockGuard<FilePrivateData>,
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
            (root_inode.clone(), devpts)
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

    // 在 master/slave 两端记录 devpts 根节点信息，便于关闭时回收 /dev/pts/ 下的节点
    if let Some(mnt_inode) = pts_root_inode.downcast_arc::<MountFSInode>() {
        let hook = Arc::new(PtyDevPtsLink {
            pts_root: Arc::downgrade(&mnt_inode),
            index,
            freed: AtomicBool::new(false),
        });
        tty.set_private_fields(hook.clone());
        if let Some(slave) = tty.core().link() {
            slave.set_private_fields(hook);
        }
    }

    ptm_driver().driver_funcs().open(core)?;

    Ok(())
}
