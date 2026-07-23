use core::fmt::Debug;

use alloc::sync::Arc;
use system_error::SystemError;

use crate::filesystem::vfs::file::FileFlags;

use super::{
    termios::Termios,
    tty_core::{TtyCore, TtyFlag},
};

pub mod ntty;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtyLdiscDrainResult {
    Drained,
    NeedWriteRoom(usize),
}

#[derive(Debug, Clone, Copy)]
pub struct TtyLdiscFileContext {
    pub flags: FileFlags,
    pub hangup_generation: usize,
}

pub trait TtyLineDiscipline: Sync + Send + Debug {
    fn open(&self, tty: Arc<TtyCore>) -> Result<(), SystemError>;
    fn close(&self, tty: Arc<TtyCore>) -> Result<(), SystemError>;
    fn flush_buffer(&self, tty: Arc<TtyCore>) -> Result<(), SystemError>;
    fn flush_output(&self, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        let ret = tty.core().driver().driver_funcs().flush_buffer(tty.core());
        if ret != Err(SystemError::ENOSYS) {
            ret?;
        }
        tty.core().write_wq().wakeup_all();
        Ok(())
    }

    /// Drain pending ldisc output (opost + echo), blocking until complete.
    ///
    /// Returns the exact minimum driver write room needed to make progress.
    ///
    /// Callers MUST loop on `NeedWriteRoom`: wait for the requested room,
    /// then call `drain_output` again until it returns `Drained`.  See
    /// `core_set_termios` for the canonical retry pattern.
    ///
    /// # Default implementation
    ///
    /// Returns `Drained`.  **Line disciplines that have their own output
    /// queues (opost / echo) MUST override this method.**  The default
    /// causes TCSADRAIN to silently skip draining for undiscovered ldiscs.
    fn drain_output(&self, _tty: Arc<TtyCore>) -> Result<TtyLdiscDrainResult, SystemError> {
        Ok(TtyLdiscDrainResult::Drained)
    }

    /// Whether this line discipline still owns output that has not yet been
    /// accepted by the driver. This must not sleep: wait-queue predicates use
    /// it to notice progress made by `write_wakeup` even when that callback
    /// consumes all newly available driver room.
    fn output_pending(&self) -> bool {
        false
    }

    /// ## tty行规程循环读取函数
    ///
    /// ### 参数
    /// - tty: 操作的tty
    /// - buf: 数据将被读取到buf
    /// - len: 读取的字节长度
    /// - cookie: 表示是否是继续上次的读，第一次读取应该传入false
    /// - offset: 读取的偏移量
    fn read(
        &self,
        tty: Arc<TtyCore>,
        buf: &mut [u8],
        len: usize,
        cookie: &mut bool,
        offset: usize,
        file_context: TtyLdiscFileContext,
    ) -> Result<usize, SystemError>;
    fn write(
        &self,
        tty: Arc<TtyCore>,
        buf: &[u8],
        len: usize,
        file_context: TtyLdiscFileContext,
    ) -> Result<usize, SystemError>;
    fn ioctl(&self, tty: Arc<TtyCore>, cmd: u32, arg: usize) -> Result<usize, SystemError>;

    /// ### 设置termios后更新行规程状态
    ///
    /// - old: 之前的termios，如果为None则表示第一次设置
    fn set_termios(&self, tty: Arc<TtyCore>, old: Option<Termios>) -> Result<(), SystemError>;

    fn poll(&self, tty: Arc<TtyCore>) -> Result<usize, SystemError>;
    fn hangup(&self, tty: Arc<TtyCore>) -> Result<(), SystemError>;

    fn receive_room(&self, _tty: Arc<TtyCore>) -> usize {
        usize::MAX
    }

    /// ## 接收数据
    fn receive_buf(
        &self,
        tty: Arc<TtyCore>,
        buf: &[u8],
        flags: Option<&[u8]>,
        count: usize,
    ) -> Result<usize, SystemError>;

    /// ## 接收数据
    fn receive_buf2(
        &self,
        tty: Arc<TtyCore>,
        buf: &[u8],
        flags: Option<&[u8]>,
        count: usize,
    ) -> Result<usize, SystemError>;

    /// Receive input while the caller already holds this TTY's termios read
    /// semaphore. PTY uses this to lock the peer before marking a direction
    /// as actively draining, avoiding nested reader acquisition.
    fn receive_buf2_termios_locked(
        &self,
        tty: Arc<TtyCore>,
        buf: &[u8],
        flags: Option<&[u8]>,
        count: usize,
    ) -> Result<usize, SystemError> {
        self.receive_buf2(tty, buf, flags, count)
    }

    /// ## 唤醒线路写者
    fn write_wakeup(&self, _tty: &TtyCore) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum LineDisciplineType {
    NTty = 0,
}

impl LineDisciplineType {
    /// Convert a raw c_line ABI byte to a LineDisciplineType.
    ///
    /// NOTE: this accepts u8 rather than LineDisciplineType, so adding
    /// new enum variants (e.g. `Ppp = 1`) will NOT trigger a compiler
    /// error here.  When extending the enum, update this match arm
    /// to map the new variant(s).
    pub fn from_line(line: u8) -> Self {
        match line {
            0 => Self::NTty,
            // Unknown / unsupported line disciplines fall back to NTty,
            // matching Linux behaviour (N_TTY is the default).
            _ => Self::NTty,
        }
    }
}

pub struct TtyLdiscManager;

impl TtyLdiscManager {
    /// ## 为tty初始化ldisc
    ///
    /// ### 参数
    /// - tty：需要设置的tty
    /// - o_tty: other tty 用于pty pair
    #[inline(never)]
    pub fn ldisc_setup(tty: Arc<TtyCore>, o_tty: Option<Arc<TtyCore>>) -> Result<(), SystemError> {
        let ld = tty.ldisc();

        let ret = ld.open(tty);
        if let Err(err) = ret {
            if err == SystemError::ENOSYS {
                return Err(err);
            }
        }

        // 处理PTY
        if let Some(o_tty) = o_tty {
            let ld = o_tty.ldisc();

            let ret: Result<(), SystemError> = ld.open(o_tty.clone());
            if ret.is_err() {
                o_tty.core().flags_write().remove(TtyFlag::LDISC_OPEN);
                let _ = ld.close(o_tty.clone());
            }
        }

        Ok(())
    }
}
