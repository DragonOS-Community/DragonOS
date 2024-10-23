use alloc::{string::ToString, sync::Arc};
use system_error::SystemError;

use crate::{
    driver::tty::{
        termios::{ControlCharIndex, ControlMode, InputMode, LocalMode, Termios},
        tty_core::{TtyCore, TtyCoreData, TtyFlag, TtyIoctlCmd, TtyPacketStatus},
        tty_device::TtyFilePrivateData,
        tty_driver::{TtyDriver, TtyDriverPrivateData, TtyDriverSubType, TtyOperation},
    },
    filesystem::{
        devpts::DevPtsFs,
        vfs::{
            file::FileMode, syscall::ModeType, FilePrivateData, FileType, MountFS, ROOT_INODE,
            VFS_MAX_FOLLOW_SYMLINK_TIMES,
        },
    },
    libs::spinlock::SpinLockGuard,
    mm::VirtAddr,
    net::event_poll::EPollEventType,
    syscall::user_access::UserBufferWriter,
};

use super::{ptm_driver, pts_driver, PtyCommon};

pub const NR_UNIX98_PTY_MAX: u32 = 128;

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

        if tty.core().driver().tty_driver_sub_type() == TtyDriverSubType::PtySlave {
            driver.ttys().remove(&tty.core().index());
            let pts_root_inode =
                ROOT_INODE().lookup_follow_symlink("/dev/pts", VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
            let _ = pts_root_inode.unlink(&tty.core().index().to_string());
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
    mut data: SpinLockGuard<FilePrivateData>,
    mode: &FileMode,
) -> Result<(), SystemError> {
    let pts_root_inode =
        ROOT_INODE().lookup_follow_symlink("/dev/pts", VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    let fs = pts_root_inode
        .fs()
        .as_any_ref()
        .downcast_ref::<MountFS>()
        .unwrap()
        .inner_filesystem();
    let fsinfo = fs.as_any_ref().downcast_ref::<DevPtsFs>().unwrap();

    let index = fsinfo.alloc_index()?;

    let tty = ptm_driver().init_tty_device(Some(index))?;

    // 设置privdata
    *data = FilePrivateData::Tty(TtyFilePrivateData {
        tty: tty.clone(),
        mode: *mode,
    });

    let core = tty.core();
    core.flags_write().insert(TtyFlag::PTY_LOCK);

    let _ = pts_root_inode.create(
        &index.to_string(),
        FileType::CharDevice,
        ModeType::from_bits_truncate(0x666),
    )?;

    ptm_driver().driver_funcs().open(core)?;

    Ok(())
}
