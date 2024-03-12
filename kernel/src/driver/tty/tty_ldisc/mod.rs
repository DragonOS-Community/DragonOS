use core::fmt::Debug;

use alloc::sync::Arc;
use system_error::SystemError;

use crate::filesystem::vfs::file::FileMode;

use super::{
    termios::Termios,
    tty_core::{TtyCore, TtyCoreData},
};

pub mod ntty;

pub trait TtyLineDiscipline: Sync + Send + Debug {
    fn open(&self, tty: Arc<TtyCore>) -> Result<(), SystemError>;
    fn close(&self, tty: Arc<TtyCore>) -> Result<(), SystemError>;
    fn flush_buffer(&self, tty: Arc<TtyCore>) -> Result<(), SystemError>;

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
        mode: FileMode,
    ) -> Result<usize, SystemError>;
    fn write(
        &self,
        tty: Arc<TtyCore>,
        buf: &[u8],
        len: usize,
        mode: FileMode,
    ) -> Result<usize, SystemError>;
    fn ioctl(&self, tty: Arc<TtyCore>, cmd: u32, arg: usize) -> Result<usize, SystemError>;

    /// ### 设置termios后更新行规程状态
    ///
    /// - old: 之前的termios，如果为None则表示第一次设置
    fn set_termios(&self, tty: Arc<TtyCore>, old: Option<Termios>) -> Result<(), SystemError>;

    fn poll(&self, tty: Arc<TtyCore>) -> Result<usize, SystemError>;
    fn hangup(&self, tty: Arc<TtyCore>) -> Result<(), SystemError>;

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

    /// ## 唤醒线路写者
    fn write_wakeup(&self, _tty: &TtyCoreData) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum LineDisciplineType {
    NTty = 0,
}

impl LineDisciplineType {
    pub fn from_line(line: u8) -> Self {
        match line {
            0 => Self::NTty,
            _ => {
                todo!()
            }
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
    pub fn ldisc_setup(tty: Arc<TtyCore>, _o_tty: Option<Arc<TtyCore>>) -> Result<(), SystemError> {
        let ld = tty.ldisc();

        let ret = ld.open(tty);
        if ret.is_err() {
            let err = ret.unwrap_err();
            if err == SystemError::ENOSYS {
                return Err(err);
            }
        }

        // TODO: 处理PTY

        Ok(())
    }
}
