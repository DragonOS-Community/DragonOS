use core::{
    fmt::Debug,
    sync::atomic::{AtomicBool, AtomicUsize},
};

use alloc::{collections::LinkedList, string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    driver::serial::serial8250::send_to_default_serial8250_port,
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockUpgradableGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::EventWaitQueue,
    },
    mm::VirtAddr,
    net::event_poll::{EPollEventType, EPollItem},
    process::Pid,
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};

use super::{
    termios::{ControlMode, PosixTermios, Termios, TtySetTermiosOpt, WindowSize},
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
            count: AtomicUsize::new(0),
            window_size: RwLock::new(WindowSize::default()),
            read_wq: EventWaitQueue::new(),
            write_wq: EventWaitQueue::new(),
            port: RwLock::new(None),
            index,
            ctrl: SpinLock::new(TtyContorlInfo::default()),
            closing: AtomicBool::new(false),
            flow: SpinLock::new(TtyFlowState::default()),
            link: None,
            epitems: SpinLock::new(LinkedList::new()),
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

    pub fn write_without_serial(&self, buf: &[u8], nr: usize) -> Result<usize, SystemError> {
        self.core
            .driver()
            .driver_funcs()
            .write(self.core(), buf, nr)
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
        if self.core.flags().contains(TtyFlag::DO_WRITE_WAKEUP) {
            let _ = self.ldisc().write_wakeup(self.core());
        }

        self.core()
            .write_wq
            .wakeup(EPollEventType::EPOLLOUT.bits() as u64);
    }

    pub fn tty_mode_ioctl(tty: Arc<TtyCore>, cmd: u32, arg: usize) -> Result<usize, SystemError> {
        let real_tty;
        let core = tty.core();
        if core.driver().tty_driver_type() == TtyDriverType::Pty
            && core.driver().tty_driver_sub_type() == TtyDriverSubType::PtyMaster
        {
            real_tty = core.link().unwrap();
        } else {
            real_tty = tty;
        }
        match cmd {
            TtyIoctlCmd::TCGETS => {
                let termios = PosixTermios::from_kernel_termios(real_tty.core.termios().clone());
                let mut user_writer = UserBufferWriter::new(
                    VirtAddr::new(arg).as_ptr::<PosixTermios>(),
                    core::mem::size_of::<PosixTermios>(),
                    true,
                )?;

                user_writer.copy_one_to_user(&termios, 0)?;
                return Ok(0);
            }
            TtyIoctlCmd::TCSETS => {
                return TtyCore::core_set_termios(
                    real_tty,
                    VirtAddr::new(arg),
                    TtySetTermiosOpt::TERMIOS_OLD,
                );
            }
            TtyIoctlCmd::TCSETSW => {
                return TtyCore::core_set_termios(
                    real_tty,
                    VirtAddr::new(arg),
                    TtySetTermiosOpt::TERMIOS_WAIT | TtySetTermiosOpt::TERMIOS_OLD,
                );
            }
            _ => {
                return Err(SystemError::ENOIOCTLCMD);
            }
        }
    }

    pub fn core_set_termios(
        tty: Arc<TtyCore>,
        arg: VirtAddr,
        opt: TtySetTermiosOpt,
    ) -> Result<usize, SystemError> {
        #[allow(unused_assignments)]
        // TERMIOS_TERMIO下会用到
        let mut tmp_termios = tty.core().termios().clone();

        if opt.contains(TtySetTermiosOpt::TERMIOS_TERMIO) {
            todo!()
        } else {
            let user_reader = UserBufferReader::new(
                arg.as_ptr::<PosixTermios>(),
                core::mem::size_of::<PosixTermios>(),
                true,
            )?;

            let mut term = PosixTermios::default();
            user_reader.copy_one_from_user(&mut term, 0)?;

            tmp_termios = term.to_kernel_termios();
        }

        if opt.contains(TtySetTermiosOpt::TERMIOS_FLUSH) {
            let ld = tty.ldisc();
            let _ = ld.flush_buffer(tty.clone());
        }

        if opt.contains(TtySetTermiosOpt::TERMIOS_WAIT) {
            // TODO
        }

        TtyCore::set_termios_next(tty, tmp_termios)?;
        Ok(0)
    }

    pub fn set_termios_next(tty: Arc<TtyCore>, new_termios: Termios) -> Result<(), SystemError> {
        let mut termios = tty.core().termios_write();

        let old_termios = termios.clone();
        *termios = new_termios;
        let tmp = termios.control_mode;
        termios.control_mode ^= (tmp ^ old_termios.control_mode) & ControlMode::ADDRB;

        let ret = tty.set_termios(tty.clone(), old_termios);
        if ret.is_err() {
            termios.control_mode &= ControlMode::HUPCL | ControlMode::CREAD | ControlMode::CLOCAL;
            termios.control_mode |= old_termios.control_mode
                & !(ControlMode::HUPCL | ControlMode::CREAD | ControlMode::CLOCAL);
            termios.input_speed = old_termios.input_speed;
            termios.output_speed = old_termios.output_speed;
        }

        drop(termios);
        let ld = tty.ldisc();
        ld.set_termios(tty, Some(old_termios))?;

        Ok(())
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
    count: AtomicUsize,
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
    /// 链接tty
    link: Option<Arc<TtyCore>>,
    /// epitems
    epitems: SpinLock<LinkedList<Arc<EPollItem>>>,
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
        self.flags.read_irqsave().clone()
    }

    #[inline]
    pub fn termios(&self) -> RwLockReadGuard<'_, Termios> {
        self.termios.read_irqsave()
    }

    #[inline]
    pub fn termios_write(&self) -> RwLockWriteGuard<Termios> {
        self.termios.write_irqsave()
    }

    #[inline]
    pub fn set_termios(&self, termios: Termios) {
        let mut termios_guard = self.termios_write();
        *termios_guard = termios;
    }

    #[inline]
    pub fn add_count(&self) {
        self.count
            .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
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
    pub fn window_size(&self) -> RwLockReadGuard<WindowSize> {
        self.window_size.read()
    }

    #[inline]
    pub fn is_closing(&self) -> bool {
        self.closing.load(core::sync::atomic::Ordering::SeqCst)
    }

    #[inline]
    pub fn vc_data_irqsave(&self) -> SpinLockGuard<VirtualConsoleData> {
        VIRT_CONSOLES[self.index].lock_irqsave()
    }

    #[inline]
    pub fn link(&self) -> Option<Arc<TtyCore>> {
        self.link.clone()
    }

    #[inline]
    pub fn add_epitem(&self, epitem: Arc<EPollItem>) {
        self.epitems.lock().push_back(epitem)
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
        send_to_default_serial8250_port(buf);
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

    #[inline]
    fn start(&self, tty: &TtyCoreData) -> Result<(), SystemError> {
        return self.core().tty_driver.driver_funcs().start(tty);
    }

    #[inline]
    fn stop(&self, tty: &TtyCoreData) -> Result<(), SystemError> {
        return self.core().tty_driver.driver_funcs().stop(tty);
    }

    #[inline]
    fn ioctl(&self, tty: Arc<TtyCore>, cmd: u32, arg: usize) -> Result<(), SystemError> {
        return self.core().tty_driver.driver_funcs().ioctl(tty, cmd, arg);
    }

    #[inline]
    fn chars_in_buffer(&self) -> usize {
        return self.core().tty_driver.driver_funcs().chars_in_buffer();
    }

    #[inline]
    fn set_termios(&self, tty: Arc<TtyCore>, old_termios: Termios) -> Result<(), SystemError> {
        return self
            .core()
            .tty_driver
            .driver_funcs()
            .set_termios(tty, old_termios);
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

pub struct TtyIoctlCmd;

#[allow(dead_code)]
impl TtyIoctlCmd {
    /// 获取终端参数
    pub const TCGETS: u32 = 0x5401;
    /// 设置终端参数
    pub const TCSETS: u32 = 0x5402;
    /// 设置终端参数并等待所有输出完成
    pub const TCSETSW: u32 = 0x5403;
    /// 设置终端参数并且等待所有输出完成，但在这之前将终端清空
    pub const TCSETSF: u32 = 0x5404;
    /// 获取终端参数
    pub const TCGETA: u32 = 0x5405;
    /// 设置终端参数
    pub const TCSETA: u32 = 0x5406;
    /// 设置终端参数并等待所有输出完成
    pub const TCSETAW: u32 = 0x5407;
    /// 设置终端参数并且等待所有输出完成，但在这之前将终端清空
    pub const TCSETAF: u32 = 0x5408;
    /// 发送零字节，等待所有输出完成
    pub const TCSBRK: u32 = 0x5409;
    /// 控制终端的流控
    pub const TCXONC: u32 = 0x540A;
    /// 刷新输入/输出缓冲区或者丢弃输入缓冲区
    pub const TCFLSH: u32 = 0x540B;
    /// 设置设备为独占模式
    pub const TIOCEXCL: u32 = 0x540C;
    /// 设置设备为非独占模式
    pub const TIOCNXCL: u32 = 0x540D;
    /// 设置当前进程的控制终端
    pub const TIOCSCTTY: u32 = 0x540E;
    /// 获取前台进程组
    pub const TIOCGPGRP: u32 = 0x540F;
    ///设置前台进程组
    pub const TIOCSPGRP: u32 = 0x5410;
    /// 获取输出队列的字节数
    pub const TIOCOUTQ: u32 = 0x5411;
    /// 模拟从终端输入字符
    pub const TIOCSTI: u32 = 0x5412;
    /// 获取窗口大小
    pub const TIOCGWINSZ: u32 = 0x5413;
    /// 设置窗口大小
    pub const TIOCSWINSZ: u32 = 0x5414;
    /// 获取终端控制信号的状态
    pub const TIOCMGET: u32 = 0x5415;
    /// 设置终端控制信号的位
    pub const TIOCMBIS: u32 = 0x5416;
    /// 清除终端控制信号的位
    pub const TIOCMBIC: u32 = 0x5417;
    /// 设置终端控制信号的状态
    pub const TIOCMSET: u32 = 0x5418;
    /// 获取软件载波状态
    pub const TIOCGSOFTCAR: u32 = 0x5419;
    /// 设置软件载波状态
    pub const TIOCSSOFTCAR: u32 = 0x541A;
    /// 获取输入队列的字节数
    pub const FIONREAD: u32 = 0x541B;
    /// Linux 特有命令
    pub const TIOCLINUX: u32 = 0x541C;
    /// 获取控制台设备
    pub const TIOCCONS: u32 = 0x541D;
    /// 获取串行设备参数
    pub const TIOCGSERIAL: u32 = 0x541E;
    /// 设置串行设备参数
    pub const TIOCSSERIAL: u32 = 0x541F;
    /// 设置套接字的报文模式
    pub const TIOCPKT: u32 = 0x5420;
    /// 设置非阻塞 I/O
    pub const FIONBIO: u32 = 0x5421;
    /// 清除控制终端
    pub const TIOCNOTTY: u32 = 0x5422;
    /// 设置终端线路驱动器
    pub const TIOCSETD: u32 = 0x5423;
    /// 获取终端线路驱动器
    pub const TIOCGETD: u32 = 0x5424;
    /// 发送终止条件
    pub const TCSBRKP: u32 = 0x5425;
    /// 开始发送零比特
    pub const TIOCSBRK: u32 = 0x5427;
    /// 停止发送零比特
    pub const TIOCCBRK: u32 = 0x5428;
    /// Return the session ID of FD
    pub const TIOCGSID: u32 = 0x5429;
}
