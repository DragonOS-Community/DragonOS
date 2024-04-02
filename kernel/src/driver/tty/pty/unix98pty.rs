use alloc::{string::ToString, sync::Arc};
use system_error::SystemError;

use crate::{
    driver::tty::{
        termios::Termios,
        tty_core::{TtyCore, TtyCoreData, TtyFlag, TtyIoctlCmd, TtyPacketStatus},
        tty_device::{TtyDevice, TtyFilePrivateData},
        tty_driver::{TtyDriver, TtyDriverPrivateData, TtyDriverSubType, TtyOperation},
        tty_port::{DefaultTtyPort, TtyPort},
    },
    filesystem::vfs::{
        file::FileMode, syscall::ModeType, FilePrivateData, FileType, ROOT_INODE,
        VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    mm::VirtAddr,
    net::event_poll::EPollEventType,
    syscall::user_access::UserBufferWriter,
};

use super::{PtyCommon, PTM_DRIVER, PTS_DRIVER};

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
            return Err(SystemError::ENOSYS);
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

    fn set_termios(&self, tty: Arc<TtyCore>, _old_termios: Termios) -> Result<(), SystemError> {
        let core = tty.core();
        if core.driver().tty_driver_sub_type() != TtyDriverSubType::PtySlave {
            return Err(SystemError::ENOSYS);
        }
        todo!()
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
            return PTS_DRIVER
                .ttys()
                .get(&index)
                .map(|x| x.clone())
                .ok_or(SystemError::ENODEV);
        }

        return Err(SystemError::ENOSYS);
    }

    fn close(&self, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        let driver = tty.core().driver();

        driver.ttys().remove(&tty.core().index());
        if tty.core().driver().tty_driver_sub_type() == TtyDriverSubType::PtySlave {
            let pts_root_inode =
                ROOT_INODE().lookup_follow_symlink("/dev/pts", VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
            pts_root_inode.unlink(&tty.core().index().to_string())?;
        }

        Ok(())
    }
}

pub fn ptmx_open(data: &mut FilePrivateData, mode: &FileMode) -> Result<(), SystemError> {
    let pts_root_inode =
        ROOT_INODE().lookup_follow_symlink("/dev/pts", VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
    // let fsinfo = pts_root_inode
    //     .fs()
    //     .downcast_arc::<MountFS>()
    //     .unwrap()
    //     .inner_filesystem()
    //     .downcast_arc::<DevPtsFs>()
    //     .unwrap();

    // let index = fsinfo.alloc_index()?;
    let index = 1;

    let tty = TtyDriver::init_tty_device(PTM_DRIVER.clone(), index)?;

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

    PTM_DRIVER.driver_funcs().open(core)?;

    Ok(())
}
