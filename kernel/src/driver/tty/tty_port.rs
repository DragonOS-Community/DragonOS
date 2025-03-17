use core::fmt::Debug;

use alloc::sync::{Arc, Weak};
use kdepends::thingbuf::mpsc;
use system_error::SystemError;

use crate::{
    libs::spinlock::{SpinLock, SpinLockGuard},
    net::event_poll::EventPoll,
};

use super::tty_core::TtyCore;

const TTY_PORT_BUFSIZE: usize = 4096;

#[allow(dead_code)]
#[derive(Debug)]
pub struct TtyPortData {
    flags: i32,
    iflags: TtyPortState,
    sender: mpsc::Sender<u8>,
    receiver: mpsc::Receiver<u8>,
    tty: Weak<TtyCore>,
    /// 内部tty，即与port直接相连的
    internal_tty: Weak<TtyCore>,
}

impl Default for TtyPortData {
    fn default() -> Self {
        Self::new()
    }
}

impl TtyPortData {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel::<u8>(TTY_PORT_BUFSIZE);
        Self {
            flags: 0,
            iflags: TtyPortState::Initialized,
            sender,
            receiver,
            tty: Weak::new(),
            internal_tty: Weak::new(),
        }
    }

    pub fn internal_tty(&self) -> Option<Arc<TtyCore>> {
        self.internal_tty.upgrade()
    }

    pub fn tty(&self) -> Option<Arc<TtyCore>> {
        self.tty.upgrade()
    }
}

#[allow(dead_code)]
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TtyPortState {
    Initialized,
    Suspended,
    Active,
    CtsFlow,
    CheckCD,
    KOPENED,
}

pub trait TtyPort: Sync + Send + Debug {
    fn port_data(&self) -> SpinLockGuard<TtyPortData>;

    /// 获取Port的状态
    fn state(&self) -> TtyPortState {
        self.port_data().iflags
    }

    /// 为port设置tty
    fn setup_internal_tty(&self, tty: Weak<TtyCore>) {
        self.port_data().internal_tty = tty;
    }

    /// 作为客户端的tty ports接收数据
    fn receive_buf(&self, buf: &[u8], _flags: &[u8], count: usize) -> Result<usize, SystemError> {
        let tty = self.port_data().internal_tty().unwrap();

        let ld = tty.ldisc();

        let ret = ld.receive_buf2(tty.clone(), buf, None, count);

        if let Err(SystemError::ENOSYS) = ret {
            return ld.receive_buf(tty, buf, None, count);
        }

        EventPoll::wakeup_epoll(tty.core().eptiems(), None)?;

        ret
    }
}

#[derive(Debug)]
pub struct DefaultTtyPort {
    port_data: SpinLock<TtyPortData>,
}

impl DefaultTtyPort {
    pub fn new() -> Self {
        Self {
            port_data: SpinLock::new(TtyPortData::new()),
        }
    }
}

impl TtyPort for DefaultTtyPort {
    fn port_data(&self) -> SpinLockGuard<TtyPortData> {
        self.port_data.lock_irqsave()
    }
}
