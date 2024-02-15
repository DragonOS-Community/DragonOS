use core::fmt::Debug;

use alloc::sync::{Arc, Weak};
use kdepends::thingbuf::mpsc;
use system_error::SystemError;

use crate::libs::spinlock::{SpinLock, SpinLockGuard};

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
        }
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
    fn setup_tty(&self, tty: Weak<TtyCore>) {
        self.port_data().tty = tty;
    }

    /// 作为客户端的tty ports接收数据
    fn receive_buf(&self, buf: &[u8], _flags: &[u8], count: usize) -> Result<usize, SystemError> {
        let tty = self.port_data().tty.upgrade().unwrap();

        let ld = tty.ldisc();

        let ret = ld.receive_buf2(tty.clone(), buf, None, count);
        if ret.is_err() && ret.clone().unwrap_err() == SystemError::ENOSYS {
            return ld.receive_buf(tty, buf, None, count);
        }

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
