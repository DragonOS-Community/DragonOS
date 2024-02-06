use core::{fmt::Debug, sync::atomic::AtomicBool};

use alloc::{string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockUpgradableGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::EventWaitQueue,
    },
    net::event_poll::EPollEventType,
    process::Pid,
};

use super::{
    termios::{Termios, WindowSize},
    tty_driver::{TtyDriver, TtyDriverSubType, TtyDriverType, TtyOperation},
    tty_ldisc::{
        ntty::{NTtyData, NTtyLinediscipline},
        TtyLineDiscipline,
    },
    tty_port::TtyPort,
    virtual_terminal::{virtual_console::VirtualConsoleData, VIRT_CONSOLES},
};

#[derive(Debug)]
pub struct TtyCore {
    core: TtyCoreData,
    /// 线路规程函数集
    line_discipline: Arc<dyn TtyLineDiscipline>,
}

impl TtyCore {
    pub fn new(driver: Arc<TtyDriver>, index: usize) -> Arc<Self> {
        let name = driver.tty_line_name(index);
        let termios = driver.init_termios();
        let core = TtyCoreData {
            tty_driver: driver,
            termios: RwLock::new(termios),
            name,
            flags: RwLock::new(TtyFlag::empty()),
            count: RwLock::new(0),
            window_size: RwLock::new(WindowSize::default()),
            read_wq: EventWaitQueue::new(),
            write_wq: EventWaitQueue::new(),
            port: RwLock::new(None),
            index,
            ctrl: SpinLock::new(TtyContorlInfo::default()),
            closing: AtomicBool::new(false),
            flow: SpinLock::new(TtyFlowState::default()),
        };

        return Arc::new(Self {
            core,
            line_discipline: Arc::new(NTtyLinediscipline {
                data: SpinLock::new(NTtyData::new()),
            }),
        });
    }

    #[inline]
    pub fn core(&self) -> &TtyCoreData {
        return &self.core;
    }

    #[inline]
    pub fn ldisc(&self) -> Arc<dyn TtyLineDiscipline> {
        self.line_discipline.clone()
    }

    pub fn reopen(&self) -> Result<(), SystemError> {
        let tty_core = self.core();
        let driver = tty_core.driver();

        if driver.tty_driver_type() == TtyDriverType::Pty
            && driver.tty_driver_sub_type() == TtyDriverSubType::PtyMaster
        {
            return Err(SystemError::EIO);
        }

        // if *tty_core.count.read() == 0 {
        //     return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        // }

        // TODO 判断flags

        tty_core.add_count();

        Ok(())
    }

    #[inline]
    pub fn set_port(&self, port: Arc<dyn TtyPort>) {
        *self.core.port.write() = Some(port);
    }

    pub fn tty_start(&self) {
        let mut flow = self.core.flow.lock_irqsave();
        if !flow.stopped || flow.tco_stopped {
            return;
        }

        flow.stopped = false;
        let _ = self.start(self.core());
        self.tty_wakeup();
    }

    pub fn tty_stop(&self) {
        let mut flow = self.core.flow.lock_irqsave();
        if flow.stopped {
            return;
        }
        flow.stopped = true;

        let _ = self.stop(self.core());
    }

    pub fn tty_wakeup(&self) {
        if self.core.flags.read().contains(TtyFlag::DO_WRITE_WAKEUP) {
            let _ = self.ldisc().write_wakeup(self.core());
        }

        self.core()
            .write_wq
            .wakeup(EPollEventType::EPOLLOUT.bits() as u64);
    }
}

#[derive(Debug)]
pub struct TtyContorlInfo {
    /// 前台进程pid
    pub session: Option<Pid>,
    /// 前台进程组id
    pub pgid: Option<Pid>,

    /// packet模式下使用，目前未用到
    pub pktstatus: u8,
    pub packet: bool,
}

impl Default for TtyContorlInfo {
    fn default() -> Self {
        Self {
            session: None,
            pgid: None,
            pktstatus: Default::default(),
            packet: Default::default(),
        }
    }
}

#[derive(Debug, Default)]
pub struct TtyCoreWriteData {
    /// 写缓冲区
    pub write_buf: Vec<u8>,
    /// 写入数量
    pub write_cnt: usize,
}

#[derive(Debug, Default)]
pub struct TtyFlowState {
    /// 表示流控是否被停止
    pub stopped: bool,
    /// 表示 TCO（Transmit Continuous Operation）流控是否被停止
    pub tco_stopped: bool,
}

#[derive(Debug)]
pub struct TtyCoreData {
    tty_driver: Arc<TtyDriver>,
    termios: RwLock<Termios>,
    name: String,
    flags: RwLock<TtyFlag>,
    /// 在初始化时即确定不会更改，所以这里不用加锁
    index: usize,
    count: RwLock<usize>,
    /// 窗口大小
    window_size: RwLock<WindowSize>,
    /// 读等待队列
    read_wq: EventWaitQueue,
    /// 写等待队列
    write_wq: EventWaitQueue,
    /// 端口
    port: RwLock<Option<Arc<dyn TtyPort>>>,
    /// 前台进程
    ctrl: SpinLock<TtyContorlInfo>,
    /// 是否正在关闭
    closing: AtomicBool,
    /// 流控状态
    flow: SpinLock<TtyFlowState>,
}

impl TtyCoreData {
    #[inline]
    pub fn driver(&self) -> Arc<TtyDriver> {
        self.tty_driver.clone()
    }

    #[inline]
    pub fn flow_irqsave(&self) -> SpinLockGuard<TtyFlowState> {
        self.flow.lock_irqsave()
    }

    #[inline]
    pub fn port(&self) -> Option<Arc<dyn TtyPort>> {
        self.port.read().clone()
    }

    #[inline]
    pub fn index(&self) -> usize {
        self.index
    }

    #[inline]
    pub fn name(&self) -> String {
        self.name.clone()
    }

    #[inline]
    pub fn flags(&self) -> TtyFlag {
        self.flags.read().clone()
    }

    #[inline]
    pub fn termios(&self) -> RwLockReadGuard<'_, Termios> {
        self.termios.read()
    }

    #[inline]
    pub fn termios_write(&self) -> RwLockWriteGuard<Termios> {
        self.termios.write()
    }

    #[inline]
    pub fn data_set_termios(&self, termios: Termios) {
        let mut termios_guard = self.termios.write();
        *termios_guard = termios;
    }

    #[inline]
    pub fn add_count(&self) {
        let mut guard = self.count.write();
        *guard += 1;
    }

    #[inline]
    pub fn read_wq(&self) -> &EventWaitQueue {
        &self.read_wq
    }

    #[inline]
    pub fn write_wq(&self) -> &EventWaitQueue {
        &self.write_wq
    }

    #[inline]
    pub fn contorl_info_irqsave(&self) -> SpinLockGuard<TtyContorlInfo> {
        self.ctrl.lock_irqsave()
    }

    #[inline]
    pub fn window_size_upgradeable(&self) -> RwLockUpgradableGuard<WindowSize> {
        self.window_size.upgradeable_read()
    }

    #[inline]
    pub fn is_closing(&self) -> bool {
        self.closing.load(core::sync::atomic::Ordering::SeqCst)
    }

    #[inline]
    pub fn vc_data(&self) -> SpinLockGuard<VirtualConsoleData> {
        VIRT_CONSOLES[self.index].lock()
    }

    #[inline]
    pub fn vc_data_irqsave(&self) -> SpinLockGuard<VirtualConsoleData> {
        VIRT_CONSOLES[self.index].lock_irqsave()
    }
}

/// TTY 核心接口，不同的tty需要各自实现这个trait
pub trait TtyCoreFuncs: Debug + Send + Sync {}

impl TtyOperation for TtyCore {
    #[inline]
    fn open(&self, tty: &TtyCoreData) -> Result<(), SystemError> {
        return self.core().tty_driver.driver_funcs().open(tty);
    }

    #[inline]
    fn write_room(&self, tty: &TtyCoreData) -> usize {
        return self.core().tty_driver.driver_funcs().write_room(tty);
    }

    #[inline]
    fn write(&self, tty: &TtyCoreData, buf: &[u8], nr: usize) -> Result<usize, SystemError> {
        return self.core().tty_driver.driver_funcs().write(tty, buf, nr);
    }

    #[inline]
    fn flush_chars(&self, tty: &TtyCoreData) {
        self.core().tty_driver.driver_funcs().flush_chars(tty);
    }

    #[inline]
    fn put_char(&self, tty: &TtyCoreData, ch: u8) -> Result<(), SystemError> {
        return self.core().tty_driver.driver_funcs().put_char(tty, ch);
    }

    #[inline]
    fn install(&self, driver: Arc<TtyDriver>, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        return self.core().tty_driver.driver_funcs().install(driver, tty);
    }

    fn start(&self, tty: &TtyCoreData) -> Result<(), SystemError> {
        return self.core().tty_driver.driver_funcs().start(tty);
    }

    fn stop(&self, tty: &TtyCoreData) -> Result<(), SystemError> {
        return self.core().tty_driver.driver_funcs().stop(tty);
    }
}

bitflags! {
    pub struct TtyFlag: u32 {
        /// 终端被节流
        const THROTTLED		= 1 << 0;
        /// 终端输入输出错误状态
        const IO_ERROR		= 1 << 1;
        /// 终端的其他一方已关闭
        const OTHER_CLOSED	= 1 << 2;
        /// 终端处于独占状态
        const EXCLUSIVE		= 1 << 3;
        /// 终端执行写唤醒操作
        const DO_WRITE_WAKEUP	= 1 << 5;
        /// 终端线路驱动程序已打开
        const LDISC_OPEN		= 1 << 11;
        /// 终端伪终端设备已锁定
        const PTY_LOCK		= 1 << 16;
        /// 终端禁用写分裂操作
        const NO_WRITE_SPLIT	= 1 << 17;
        /// 终端挂断（挂起）状态
        const HUPPED		= 1 << 18;
        /// 终端正在挂断（挂起）
        const HUPPING		= 1 << 19;
        /// 终端线路驱动程序正在更改
        const LDISC_CHANGING	= 1 << 20;
        /// 终端线路驱动程序已停止
        const LDISC_HALTED	= 1 << 22;
    }
}

#[derive(Debug, PartialEq)]
pub enum EchoOperation {
    /// 开始特殊操作。
    Start,
    /// 向后移动光标列。
    MoveBackCol,
    /// 设置规范模式下的列位置。
    SetCanonCol,
    /// 擦除制表符。
    EraseTab,

    Undefined(u8),
}

impl EchoOperation {
    pub fn from_u8(num: u8) -> EchoOperation {
        match num {
            0xff => Self::Start,
            0x80 => Self::MoveBackCol,
            0x81 => Self::SetCanonCol,
            0x82 => Self::EraseTab,
            _ => Self::Undefined(num),
        }
    }

    pub fn to_u8(&self) -> u8 {
        match *self {
            EchoOperation::Start => 0xff,
            EchoOperation::MoveBackCol => 0x80,
            EchoOperation::SetCanonCol => 0x81,
            EchoOperation::EraseTab => 0x82,
            EchoOperation::Undefined(num) => num,
        }
    }
}
