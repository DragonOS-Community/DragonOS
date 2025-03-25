use alloc::boxed::Box;
use core::intrinsics::likely;
use core::ops::BitXor;

use bitmap::{traits::BitMapOps, StaticBitmap};

use alloc::sync::{Arc, Weak};
use system_error::SystemError;

use crate::{
    arch::ipc::signal::Signal,
    driver::tty::{
        termios::{ControlCharIndex, InputMode, LocalMode, OutputMode, Termios},
        tty_core::{EchoOperation, TtyCore, TtyCoreData, TtyFlag, TtyIoctlCmd, TtyPacketStatus},
        tty_driver::{TtyDriverFlag, TtyOperation},
        tty_job_control::TtyJobCtrlManager,
    },
    filesystem::vfs::file::FileMode,
    libs::{
        rwlock::RwLockReadGuard,
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::VirtAddr,
    net::event_poll::EPollEventType,
    process::{ProcessFlags, ProcessManager},
    syscall::{user_access::UserBufferWriter, Syscall},
};

use super::TtyLineDiscipline;
pub const NTTY_BUFSIZE: usize = 4096;
pub const ECHO_COMMIT_WATERMARK: usize = 256;
pub const ECHO_BLOCK: usize = 256;
pub const ECHO_DISCARD_WATERMARK: usize = NTTY_BUFSIZE - (ECHO_BLOCK + 32);

fn ntty_buf_mask(idx: usize) -> usize {
    return idx & (NTTY_BUFSIZE - 1);
}

#[derive(Debug)]
pub struct NTtyLinediscipline {
    pub data: SpinLock<NTtyData>,
}

impl NTtyLinediscipline {
    #[inline]
    pub fn disc_data(&self) -> SpinLockGuard<NTtyData> {
        self.data.lock_irqsave()
    }

    #[inline]
    pub fn disc_data_try_lock(&self) -> Result<SpinLockGuard<NTtyData>, SystemError> {
        self.data.try_lock_irqsave()
    }

    fn ioctl_helper(&self, tty: Arc<TtyCore>, cmd: u32, arg: usize) -> Result<usize, SystemError> {
        match cmd {
            TtyIoctlCmd::TCXONC => {
                todo!()
            }
            TtyIoctlCmd::TCFLSH => {
                todo!()
            }
            _ => {
                return TtyCore::tty_mode_ioctl(tty.clone(), cmd, arg);
            }
        }
    }
}

#[derive(Debug)]
pub struct NTtyData {
    /// 写者管理,tty只有一个写者，即ttydevice，所以不需要加锁
    /// 读取缓冲区的头指针，表示下一个将要接受进buf的字符的位置
    read_head: usize,
    ///  提交缓冲区的头指针，用于行规程处理
    commit_head: usize,
    /// 规范缓冲区的头指针，用于规范模式的处理
    canon_head: usize,
    /// 回显缓冲区的头指针，用于存储需要回显的字符
    echo_head: usize,
    /// 回显过程中用于提交的头指针
    echo_commit: usize,
    /// 标记回显字符的起始位置
    echo_mark: usize,

    /// 读者管理
    /// 读取字符的尾指针,即当前读取位置
    read_tail: usize,
    /// 行的起始位置
    line_start: usize,
    /// 预读字符数，用于处理控制字符
    lookahead_count: usize,

    // 更改以下六个标记时必须持有termios的锁
    /// Line-next 标志，表示下一个输入字符应当按字面处理
    lnext: bool,
    /// 擦除状态的标志
    erasing: bool,
    /// Raw 模式的标志
    raw: bool,
    /// Real raw 模式的标志
    real_raw: bool,
    /// 规范模式的标志
    icanon: bool,
    /// 是否开启echo
    echo: bool,
    ///  标志是否正在进行推送
    pushing: bool,
    /// 是否没有空间可写
    no_room: bool,

    /// 光标所在列
    cursor_column: u32,
    /// 规范模式下光标所在列
    canon_cursor_column: u32,
    /// 回显缓冲区的尾指针
    echo_tail: usize,

    /// 写者与读者共享
    read_buf: Box<[u8; NTTY_BUFSIZE]>,
    echo_buf: Box<[u8; NTTY_BUFSIZE]>,

    read_flags: StaticBitmap<NTTY_BUFSIZE>,
    char_map: StaticBitmap<256>,

    tty: Weak<TtyCore>,
}

impl NTtyData {
    #[inline(never)]
    pub fn new() -> Self {
        Self {
            read_head: 0,
            commit_head: 0,
            canon_head: 0,
            echo_head: 0,
            echo_commit: 0,
            echo_mark: 0,
            read_tail: 0,
            line_start: 0,
            lookahead_count: 0,
            lnext: false,
            erasing: false,
            raw: false,
            real_raw: false,
            icanon: false,
            pushing: false,
            echo: false,
            cursor_column: 0,
            canon_cursor_column: 0,
            echo_tail: 0,
            read_buf: Box::new([0; NTTY_BUFSIZE]),
            echo_buf: Box::new([0; NTTY_BUFSIZE]),
            read_flags: StaticBitmap::new(),
            char_map: StaticBitmap::new(),
            tty: Weak::default(),
            no_room: false,
        }
    }

    #[inline]
    pub fn read_cnt(&self) -> usize {
        self.read_head - self.read_tail
    }

    #[inline]
    pub fn read_at(&self, i: usize) -> u8 {
        let i = i & (NTTY_BUFSIZE - 1);
        self.read_buf[i]
    }

    /// ### 接收数据到NTTY
    pub fn receive_buf_common(
        &mut self,
        tty: Arc<TtyCore>,
        buf: &[u8],
        flags: Option<&[u8]>,
        mut count: usize,
        flow: bool,
    ) -> Result<usize, SystemError> {
        // 获取termios读锁
        let termios = tty.core().termios();
        let mut overflow;
        let mut n;
        let mut offset = 0;
        let mut recved = 0;
        loop {
            let tail = self.read_tail;

            let mut room = NTTY_BUFSIZE - (self.read_head - tail);
            if termios.input_mode.contains(InputMode::PARMRK) {
                room = (room + 2) / 3;
            }

            room -= 1;
            if room == 0 || room > NTTY_BUFSIZE {
                // 可能溢出
                overflow = self.icanon && self.canon_head == tail;
                if room > NTTY_BUFSIZE && overflow {
                    self.read_head -= 1;
                }
                self.no_room = flow && !overflow;
                room = if overflow { !0 } else { 0 }
            } else {
                overflow = false;
            }

            n = count.min(room);
            if n == 0 {
                break;
            }

            if !overflow {
                if let Some(flags) = flags {
                    self.receive_buf(tty.clone(), &buf[offset..], Some(&flags[offset..]), n);
                } else {
                    self.receive_buf(tty.clone(), &buf[offset..], flags, n);
                }
            }

            offset += n;

            count -= n;

            recved += n;

            if tty.core().flags().contains(TtyFlag::LDISC_CHANGING) {
                break;
            }
        }

        // TODO: throttle

        Ok(recved)
    }

    pub fn receive_buf(
        &mut self,
        tty: Arc<TtyCore>,
        buf: &[u8],
        flags: Option<&[u8]>,
        count: usize,
    ) {
        let termios = tty.core().termios();
        let preops = termios.input_mode.contains(InputMode::ISTRIP)
            || termios.input_mode.contains(InputMode::IUCLC)
            || termios.local_mode.contains(LocalMode::IEXTEN);

        let look_ahead = self.lookahead_count.min(count);
        if self.real_raw {
            self.receive_buf_real_raw(buf, count);
        } else if self.raw || (termios.local_mode.contains(LocalMode::EXTPROC) && !preops) {
            self.receive_buf_raw(buf, flags, count);
        } else if tty.core().is_closing() && !termios.local_mode.contains(LocalMode::EXTPROC) {
            todo!()
        } else {
            if look_ahead > 0 {
                self.receive_buf_standard(tty.clone(), buf, flags, look_ahead, true);
            }

            if count > look_ahead {
                self.receive_buf_standard(tty.clone(), buf, flags, count - look_ahead, false);
            }

            // 刷新echo
            self.flush_echoes(tty.clone());

            tty.flush_chars(tty.core());
        }

        self.lookahead_count -= look_ahead;

        if self.icanon && !termios.local_mode.contains(LocalMode::EXTPROC) {
            return;
        }

        self.commit_head = self.read_head;

        if self.read_cnt() > 0 {
            tty.core()
                .read_wq()
                .wakeup_any((EPollEventType::EPOLLIN | EPollEventType::EPOLLRDBAND).bits() as u64);
        }
    }

    fn receive_buf_real_raw(&mut self, buf: &[u8], mut count: usize) {
        let mut head = ntty_buf_mask(self.read_head);
        let mut n = count.min(NTTY_BUFSIZE - head);

        // 假如有一部分在队列头部，则这部分是拷贝尾部部分
        self.read_buf[head..(head + n)].copy_from_slice(&buf[0..n]);
        self.read_head += n;
        count -= n;
        let offset = n;

        // 假如有一部分在队列头部，则这部分是拷贝头部部分
        head = ntty_buf_mask(self.read_head);
        n = count.min(NTTY_BUFSIZE - head);
        self.read_buf[head..(head + n)].copy_from_slice(&buf[offset..(offset + n)]);
        self.read_head += n;
    }

    fn receive_buf_raw(&mut self, buf: &[u8], flags: Option<&[u8]>, mut count: usize) {
        // TTY_NORMAL 目前这部分未做，所以先占位置而不做抽象
        let mut flag = 1;
        let mut f_offset = 0;
        let mut c_offset = 0;
        while count != 0 {
            if flags.is_some() {
                flag = flags.as_ref().unwrap()[f_offset];
                f_offset += 1;
            }

            if likely(flag == 1) {
                self.read_buf[self.read_head] = buf[c_offset];
                c_offset += 1;
                self.read_head += 1;
            } else {
                todo!()
            }

            count -= 1;
        }
    }

    pub fn flush_echoes(&mut self, tty: Arc<TtyCore>) {
        let termios = tty.core().termios();
        if !termios.local_mode.contains(LocalMode::ECHO)
            && !termios.local_mode.contains(LocalMode::ECHONL)
            || self.echo_commit == self.echo_head
        {
            return;
        }

        self.echo_commit = self.echo_head;
        drop(termios);
        let _ = self.echoes(tty);
    }

    pub fn receive_buf_standard(
        &mut self,
        tty: Arc<TtyCore>,
        buf: &[u8],
        flags: Option<&[u8]>,
        mut count: usize,
        lookahead_done: bool,
    ) {
        let termios = tty.core().termios();
        if flags.is_some() {
            todo!("ntty recv buf flags todo");
        }

        let mut offset = 0;
        while count > 0 {
            if offset >= buf.len() {
                break;
            }
            let mut c = buf[offset];
            offset += 1;

            if self.lnext {
                // 将下一个字符当做字面值处理
                self.lnext = false;
                if termios.input_mode.contains(InputMode::ISTRIP) {
                    c &= 0x7f;
                }

                if termios.input_mode.contains(InputMode::IUCLC)
                    && termios.local_mode.contains(LocalMode::IEXTEN)
                {
                    c = (c as char).to_ascii_lowercase() as u8;
                    self.receive_char(c, tty.clone())
                }

                continue;
            }

            if termios.input_mode.contains(InputMode::ISTRIP) {
                c &= 0x7f;
            }

            if termios.input_mode.contains(InputMode::IUCLC)
                && termios.local_mode.contains(LocalMode::IEXTEN)
            {
                c = (c as char).to_ascii_lowercase() as u8;
            }

            if termios.local_mode.contains(LocalMode::EXTPROC) {
                self.add_read_byte(c);
                continue;
            }

            if ((c as usize) < self.char_map.size()) && self.char_map.get(c as usize).unwrap() {
                // 特殊字符
                self.receive_special_char(c, tty.clone(), lookahead_done);
            } else {
                self.receive_char(c, tty.clone());
            }

            count -= 1;
        }
    }

    #[inline(never)]
    pub fn receive_special_char(&mut self, mut c: u8, tty: Arc<TtyCore>, lookahead_done: bool) {
        let is_flow_ctrl = self.is_flow_ctrl_char(tty.clone(), c, lookahead_done);
        let termios = tty.core().termios();

        // 启用软件流控，并且该字符已经当做软件流控字符处理
        if termios.input_mode.contains(InputMode::IXON) && is_flow_ctrl {
            return;
        }

        if termios.local_mode.contains(LocalMode::ISIG) {
            if c == termios.control_characters[ControlCharIndex::VINTR] {
                self.recv_sig_char(tty.clone(), &termios, Signal::SIGINT, c);
                return;
            }

            if c == termios.control_characters[ControlCharIndex::VQUIT] {
                self.recv_sig_char(tty.clone(), &termios, Signal::SIGQUIT, c);
                return;
            }

            if c == termios.control_characters[ControlCharIndex::VSUSP] {
                self.recv_sig_char(tty.clone(), &termios, Signal::SIGTSTP, c);
                return;
            }
        }

        let flow = tty.core().flow_irqsave();
        if flow.stopped
            && !flow.tco_stopped
            && termios.input_mode.contains(InputMode::IXON)
            && termios.input_mode.contains(InputMode::IXANY)
        {
            tty.tty_start();
            self.process_echoes(tty.clone());
        }
        drop(flow);

        if c == b'\r' {
            if termios.input_mode.contains(InputMode::IGNCR) {
                // 忽略
                return;
            }
            if termios.input_mode.contains(InputMode::ICRNL) {
                // 映射为换行
                c = b'\n';
            }
        } else if c == b'\n' && termios.input_mode.contains(InputMode::INLCR) {
            // 映射为回车
            c = b'\r';
        }

        if self.icanon {
            if c == termios.control_characters[ControlCharIndex::VERASE]
                || c == termios.control_characters[ControlCharIndex::VKILL]
                || (c == termios.control_characters[ControlCharIndex::VWERASE]
                    && termios.local_mode.contains(LocalMode::IEXTEN))
            {
                self.eraser(c, &termios);
                self.commit_echoes(tty.clone());
                return;
            }
            if c == termios.control_characters[ControlCharIndex::VLNEXT]
                && termios.local_mode.contains(LocalMode::IEXTEN)
            {
                self.lnext = true;
                if termios.local_mode.contains(LocalMode::ECHO) {
                    self.finish_erasing();
                    if termios.local_mode.contains(LocalMode::ECHOCTL) {
                        self.echo_char_raw(b'^');
                        self.echo_char_raw(8);
                        self.commit_echoes(tty.clone());
                    }
                }
                return;
            }
            if c == termios.control_characters[ControlCharIndex::VREPRINT]
                && termios.local_mode.contains(LocalMode::ECHO)
                && termios.local_mode.contains(LocalMode::IEXTEN)
            {
                let mut tail = self.canon_head;
                self.finish_erasing();
                self.echo_char(c, &termios);
                self.echo_char_raw(b'\n');
                while ntty_buf_mask(tail) != ntty_buf_mask(self.read_head) {
                    self.echo_char(self.read_buf[ntty_buf_mask(tail)], &termios);
                    tail += 1;
                }
                self.commit_echoes(tty.clone());
                return;
            }

            if c == b'\n' {
                if termios.local_mode.contains(LocalMode::ECHO)
                    || termios.local_mode.contains(LocalMode::ECHONL)
                {
                    self.echo_char_raw(b'\n');
                    self.commit_echoes(tty.clone());
                }

                self.read_flags.set(ntty_buf_mask(self.read_head), true);
                self.read_buf[ntty_buf_mask(self.read_head)] = c;
                self.read_head += 1;
                self.canon_head = self.read_head;
                tty.core().read_wq().wakeup_any(
                    (EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as u64,
                );
                return;
            }

            if c == termios.control_characters[ControlCharIndex::VEOF] {
                c = ControlCharIndex::DISABLE_CHAR;

                self.read_flags.set(ntty_buf_mask(self.read_head), true);
                self.read_buf[ntty_buf_mask(self.read_head)] = c;
                self.read_head += 1;
                self.canon_head = self.read_head;
                tty.core().read_wq().wakeup_any(
                    (EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as u64,
                );
                return;
            }

            if c == termios.control_characters[ControlCharIndex::VEOL]
                || (c == termios.control_characters[ControlCharIndex::VEOL2]
                    && termios.local_mode.contains(LocalMode::IEXTEN))
            {
                if termios.local_mode.contains(LocalMode::ECHO) {
                    if self.canon_head == self.read_head {
                        self.add_echo_byte(EchoOperation::Start.to_u8());
                        self.add_echo_byte(EchoOperation::SetCanonCol.to_u8());
                    }
                    self.echo_char(c, &termios);
                    self.commit_echoes(tty.clone());
                }

                if c == 0o377 && termios.input_mode.contains(InputMode::PARMRK) {
                    self.read_buf[ntty_buf_mask(self.read_head)] = c;
                    self.read_head += 1;
                }

                self.read_flags.set(ntty_buf_mask(self.read_head), true);
                self.read_buf[ntty_buf_mask(self.read_head)] = c;
                self.read_head += 1;
                self.canon_head = self.read_head;
                tty.core().read_wq().wakeup_any(
                    (EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as u64,
                );
                return;
            }
        }

        if termios.local_mode.contains(LocalMode::ECHO) {
            self.finish_erasing();
            if c == b'\n' {
                self.echo_char_raw(b'\n');
            } else {
                if self.canon_head == self.read_head {
                    self.add_echo_byte(EchoOperation::Start.to_u8());
                    self.add_echo_byte(EchoOperation::SetCanonCol.to_u8());
                }
                self.echo_char(c, &termios);
            }

            self.commit_echoes(tty.clone());
        }

        if c == 0o377 && termios.input_mode.contains(InputMode::PARMRK) {
            self.read_buf[ntty_buf_mask(self.read_head)] = c;
            self.read_head += 1;
        }

        self.read_buf[ntty_buf_mask(self.read_head)] = c;
        self.read_head += 1;
    }

    /// ## ntty默认eraser function
    #[inline(never)]
    fn eraser(&mut self, mut c: u8, termios: &RwLockReadGuard<Termios>) {
        if self.read_head == self.canon_head {
            return;
        }

        let erase = c == termios.control_characters[ControlCharIndex::VERASE];
        let werase = c == termios.control_characters[ControlCharIndex::VWERASE];
        let kill = !erase && !werase;

        if kill {
            if !termios.local_mode.contains(LocalMode::ECHO) {
                self.read_head = self.canon_head;
                return;
            }
            if !termios.local_mode.contains(LocalMode::ECHOK)
                || !termios.local_mode.contains(LocalMode::ECHOKE)
                || !termios.local_mode.contains(LocalMode::ECHOE)
            {
                self.read_head = self.canon_head;
                if self.erasing {
                    self.echo_char_raw(c);
                    self.erasing = false;
                }
                self.echo_char(c, termios);

                if termios.local_mode.contains(LocalMode::ECHOK) {
                    // 添加新行
                    self.echo_char_raw(b'\n');
                }
                return;
            }
        }

        let mut head;
        let mut cnt;
        while ntty_buf_mask(self.read_head) != ntty_buf_mask(self.canon_head) {
            head = self.read_head;

            loop {
                // 消除多字节字符
                head -= 1;
                c = self.read_buf[ntty_buf_mask(head)];

                if !(Self::is_continuation(c, termios)
                    && ntty_buf_mask(head) != ntty_buf_mask(self.canon_head))
                {
                    break;
                }
            }

            if Self::is_continuation(c, termios) {
                break;
            }

            if werase {
                todo!()
            }

            cnt = self.read_head - head;
            self.read_head = head;
            if termios.local_mode.contains(LocalMode::ECHO) {
                if termios.local_mode.contains(LocalMode::ECHOPRT) {
                    if !self.erasing {
                        self.echo_char_raw(b'\\');
                        self.erasing = true;
                    }
                    self.echo_char(c, termios);
                    cnt -= 1;
                    while cnt > 0 {
                        cnt -= 1;
                        head += 1;
                        self.echo_char_raw(self.read_buf[ntty_buf_mask(head)]);
                        self.add_echo_byte(EchoOperation::Start.to_u8());
                        self.add_echo_byte(EchoOperation::MoveBackCol.to_u8());
                    }
                } else if erase && !termios.local_mode.contains(LocalMode::ECHOE) {
                    self.echo_char(
                        termios.control_characters[ControlCharIndex::VERASE],
                        termios,
                    );
                } else if c == b'\t' {
                    let mut num_chars = 0;
                    let mut after_tab = false;
                    let mut tail = self.read_head;

                    while ntty_buf_mask(tail) != ntty_buf_mask(self.canon_head) {
                        tail -= 1;
                        c = self.read_buf[ntty_buf_mask(tail)];
                        if c == b'\t' {
                            after_tab = true;
                            break;
                        } else if (c as char).is_control() {
                            if termios.local_mode.contains(LocalMode::ECHOCTL) {
                                num_chars += 2;
                            }
                        } else if !Self::is_continuation(c, termios) {
                            num_chars += 1;
                        }
                    }

                    self.echo_erase_tab(num_chars, after_tab);
                } else {
                    if (c as char).is_control() && termios.local_mode.contains(LocalMode::ECHOCTL) {
                        // 8 => '\b'
                        self.echo_char_raw(8);
                        self.echo_char_raw(b' ');
                        self.echo_char_raw(8);
                    }

                    if !(c as char).is_control() || termios.local_mode.contains(LocalMode::ECHOCTL)
                    {
                        // 8 => '\b'
                        self.echo_char_raw(8);
                        self.echo_char_raw(b' ');
                        self.echo_char_raw(8);
                    }
                }
            }

            if erase {
                break;
            }
        }

        if self.read_head == self.canon_head && termios.local_mode.contains(LocalMode::ECHO) {
            self.finish_erasing();
        }
    }

    fn finish_erasing(&mut self) {
        if self.erasing {
            self.echo_char_raw(b'/');
            self.erasing = false;
        }
    }

    fn echo_erase_tab(&mut self, mut num: u8, after_tab: bool) {
        self.add_echo_byte(EchoOperation::Start.to_u8());
        self.add_echo_byte(EchoOperation::EraseTab.to_u8());

        num &= 7;

        if after_tab {
            num |= 0x80;
        }

        self.add_echo_byte(num);
    }

    /// ## 多字节字符检测
    /// 检测是否为多字节字符的后续字节
    fn is_continuation(c: u8, termios: &RwLockReadGuard<Termios>) -> bool {
        return termios.input_mode.contains(InputMode::IUTF8) && (c & 0xc0) == 0x80;
    }

    /// ## 该字符是否已经当做流控字符处理
    pub fn is_flow_ctrl_char(&mut self, tty: Arc<TtyCore>, c: u8, lookahead_done: bool) -> bool {
        let termios = tty.core().termios();

        if !(termios.control_characters[ControlCharIndex::VSTART] == c
            || termios.control_characters[ControlCharIndex::VSTOP] == c)
        {
            return false;
        }

        if lookahead_done {
            return true;
        }

        if termios.control_characters[ControlCharIndex::VSTART] == c {
            tty.tty_start();
            self.process_echoes(tty.clone());
            return true;
        } else {
            tty.tty_stop();
            return true;
        }
    }

    /// ## 接收到信号字符时的处理
    fn recv_sig_char(
        &mut self,
        tty: Arc<TtyCore>,
        termios: &RwLockReadGuard<Termios>,
        signal: Signal,
        c: u8,
    ) {
        self.input_signal(tty.clone(), termios, signal);
        if termios.input_mode.contains(InputMode::IXON) {
            tty.tty_start();
        }

        if termios.local_mode.contains(LocalMode::ECHO) {
            self.echo_char(c, termios);
            self.commit_echoes(tty);
        } else {
            self.process_echoes(tty);
        }
    }

    /// ## 处理输入信号
    pub fn input_signal(
        &mut self,
        tty: Arc<TtyCore>,
        termios: &RwLockReadGuard<Termios>,
        signal: Signal,
    ) {
        // 先处理信号
        let ctrl_info = tty.core().contorl_info_irqsave();
        let pg = ctrl_info.pgid;
        if let Some(pg) = pg {
            let _ = Syscall::kill(pg, signal as i32);
        }

        if !termios.local_mode.contains(LocalMode::NOFLSH) {
            // 重置
            self.echo_head = 0;
            self.echo_tail = 0;
            self.echo_mark = 0;
            self.echo_commit = 0;

            let _ = tty.flush_buffer(tty.core());

            self.read_head = 0;
            self.canon_head = 0;
            self.read_tail = 0;
            self.line_start = 0;

            self.erasing = false;
            self.read_flags.set_all(false);
            self.pushing = false;
            self.lookahead_count = 0;

            if tty.core().link().is_some() {
                self.packet_mode_flush(tty.core());
            }
        }
    }

    pub fn receive_char(&mut self, c: u8, tty: Arc<TtyCore>) {
        let termios = tty.core().termios();

        if termios.local_mode.contains(LocalMode::ECHO) {
            if self.erasing {
                self.add_echo_byte(b'/');
                self.erasing = false;
            }

            if self.canon_head == self.read_head {
                self.add_echo_byte(EchoOperation::Start.to_u8());
                self.add_echo_byte(EchoOperation::SetCanonCol.to_u8());
            }

            self.echo_char(c, &termios);
            self.commit_echoes(tty.clone());
        }

        if c == 0o377 && tty.core().termios().input_mode.contains(InputMode::PARMRK) {
            self.add_read_byte(c);
        }
        self.add_read_byte(c);
    }

    pub fn echo_char(&mut self, c: u8, termios: &RwLockReadGuard<Termios>) {
        if c == EchoOperation::Start.to_u8() {
            self.add_echo_byte(EchoOperation::Start.to_u8());
            self.add_echo_byte(EchoOperation::Start.to_u8());
        } else {
            if termios.local_mode.contains(LocalMode::ECHOCTL)
                && (c as char).is_control()
                && c != b'\t'
            {
                self.add_echo_byte(EchoOperation::Start.to_u8());
            }
            self.add_echo_byte(c);
        }
    }

    pub fn echo_char_raw(&mut self, c: u8) {
        if c == EchoOperation::Start.to_u8() {
            self.add_echo_byte(EchoOperation::Start.to_u8());
            self.add_echo_byte(EchoOperation::Start.to_u8());
        } else {
            self.add_echo_byte(c);
        }
    }

    /// ## 提交echobuf里的数据显示
    pub fn commit_echoes(&mut self, tty: Arc<TtyCore>) {
        let head = self.echo_head;
        self.echo_mark = head;
        let old = self.echo_commit - self.echo_tail;

        // 需要echo的字符个数
        let nr = head - self.echo_tail;

        if nr < ECHO_COMMIT_WATERMARK || nr % ECHO_BLOCK > old % ECHO_BLOCK {
            return;
        }

        self.echo_commit = head;
        let echoed = self.echoes(tty.clone());

        if echoed.is_ok() && echoed.unwrap() > 0 {
            tty.flush_chars(tty.core());
        }
    }

    pub fn add_echo_byte(&mut self, c: u8) {
        self.echo_buf[ntty_buf_mask(self.echo_head)] = c;
        self.echo_head += 1;
    }

    pub fn add_read_byte(&mut self, c: u8) {
        self.read_buf[ntty_buf_mask(self.read_head)] = c;
        self.read_head += 1;
    }

    /// ### 将read_buffer的部分值置0
    ///
    /// 只会在规范模式和禁用echo下执行
    #[inline]
    pub fn zero_buffer(&mut self, offset: usize, size: usize) {
        let offset = offset & (NTTY_BUFSIZE - 1);
        if self.icanon && !self.echo {
            let n = offset + size;
            if n > NTTY_BUFSIZE {
                for c in &mut self.read_buf[offset..NTTY_BUFSIZE] {
                    *c = 0
                }

                for c in &mut self.read_buf[0..(n - NTTY_BUFSIZE)] {
                    *c = 0
                }
            } else {
                for c in &mut self.read_buf[offset..n] {
                    *c = 0
                }
            };
        }
    }

    /// ## 从ntty中拷贝数据
    ///
    /// ### 参数
    ///
    /// ### to: 存储数据
    /// ### tail: 读取尾
    pub fn ntty_copy(
        &mut self,
        to: &mut [u8],
        tail: usize,
        n: &mut usize,
    ) -> Result<(), SystemError> {
        if to.len() < *n {
            *n = to.len();
            // return Err(SystemError::EINVAL);
        }
        if tail > NTTY_BUFSIZE {
            return Err(SystemError::EINVAL);
        }

        let size = NTTY_BUFSIZE - tail;

        if size < *n {
            // 有一部分数据在头部,则先拷贝后面部分，再拷贝头部
            // TODO: tty审计？
            to[0..size].copy_from_slice(&self.read_buf[tail..(tail + size)]);
            to[size..(*n)].copy_from_slice(&self.read_buf[0..(*n - size)]);
        } else {
            to[..*n].copy_from_slice(&self.read_buf[tail..(tail + *n)])
        }

        self.zero_buffer(tail, *n);

        Ok(())
    }

    /// ## 规范模式下跳过EOF
    pub fn canon_skip_eof(&mut self) {
        // 没有数据
        if self.read_tail == self.canon_head {
            return;
        }

        let tail = self.read_tail & (NTTY_BUFSIZE - 1);

        // 查看read_flags是否读取位置为特殊字符
        if !self.read_flags.get(tail).unwrap() {
            return;
        }

        // 确保读取位置是'\0'字符
        if self.read_buf[tail] != ControlCharIndex::DISABLE_CHAR {
            return;
        }

        // 处理该字符，将read_flagsw该位清除
        self.read_flags.set(tail, false);
        // 读取位置+1，即跳过该字符不做处理
        self.read_tail += 1;
    }

    /// ## 在规范模式（canonical mode）下从读缓冲中复制一行
    ///
    /// 一次只拷贝一行
    ///
    /// ## 参数
    /// ### dst: 存放数据
    /// ### nr: 需要拷贝的数据大小
    ///
    /// ## 返回值
    /// ### true: 表示一行未结束并且还有数据可读
    /// ### false: 一行已结束或者没有数据可读
    pub fn canon_copy_from_read_buf(
        &mut self,
        dst: &mut [u8],
        nr: &mut usize,
        offset: &mut usize,
    ) -> Result<bool, SystemError> {
        if *nr == 0 {
            return Ok(false);
        }

        let canon_head = self.canon_head;

        // 取得能够读到的字符数，即canon_head - self.read_tail和nr最小值
        let mut n = (*nr).min(canon_head - self.read_tail);

        // 获得读尾index
        let tail = self.read_tail & (NTTY_BUFSIZE - 1);

        // 避免越界，这个size才是实际读取大小
        let size = if tail + n > NTTY_BUFSIZE {
            NTTY_BUFSIZE
        } else {
            tail + n
        };

        // 找到eol的坐标
        let tmp = self.read_flags.next_index(tail);
        // 找到的话即为坐标，未找到的话即为NTTY_BUFSIZE
        let mut eol = if let Some(tmp) = tmp { tmp } else { size };
        if eol > size {
            eol = size
        }

        // 是否需要绕回缓冲区头部
        let more = n - (size - tail);

        // 是否找到eol
        let found = if eol == NTTY_BUFSIZE && more > 0 {
            // 需要返回头部
            let ret = self.read_flags.first_index();
            if let Some(tmp) = ret {
                // 在头部范围内找到eol
                if tmp < more {
                    eol = tmp;
                }
            } else {
                eol = more;
            }
            eol != more
        } else {
            // 不需要返回头部
            eol != size
        };

        n = eol - tail;
        if n > NTTY_BUFSIZE {
            // 减法溢出则加上BUFSIZE即可限制在0-NTTY_BUFSIZE内
            n += NTTY_BUFSIZE;
        }

        // 规范模式下实际扫描过的字符数，需要将eol计算在内
        let count = if found { n + 1 } else { n };

        // 表示这一行未结束
        if !found || self.read_at(eol) != ControlCharIndex::DISABLE_CHAR {
            n = count;
        }

        self.ntty_copy(&mut dst[*offset..], tail, &mut n)?;
        *nr -= n;
        *offset += n;

        if found {
            self.read_flags.set(eol, false);
        }

        self.read_tail += count;

        if found {
            if !self.pushing {
                self.line_start = self.read_tail;
            } else {
                self.pushing = false;
            }

            // todo: 审计？
            return Ok(false);
        }

        // 这里是表示没有找到eol,根据是否还有数据可读返回
        Ok(self.read_tail != canon_head)
    }

    /// ## 根据终端的模式和输入缓冲区中的数据量，判断是否可读字符
    pub fn input_available(&self, termios: RwLockReadGuard<Termios>, poll: bool) -> bool {
        // 计算最小字符数
        let amt = if poll
            && termios.control_characters[ControlCharIndex::VTIME] as u32 == 0
            && termios.control_characters[ControlCharIndex::VMIN] as u32 != 0
        {
            termios.control_characters[ControlCharIndex::VMIN] as usize
        } else {
            1
        };

        // 规范模式且非拓展
        if self.icanon && !termios.local_mode.contains(LocalMode::EXTPROC) {
            return self.canon_head != self.read_tail;
        } else {
            return (self.commit_head - self.read_tail) >= amt;
        }
    }

    /// ## 非规范模式下从read_buf读取数据
    ///
    /// ## 参数
    /// ### termios: tty对应的termioss读锁守卫
    /// ### dst: 存储读取数据
    /// ### nr: 读取长度
    ///
    /// ## 返回值
    /// ### true: 还有更多数据可读
    /// ### false: 无更多数据可读
    pub fn copy_from_read_buf(
        &mut self,
        termios: RwLockReadGuard<Termios>,
        dst: &mut [u8],
        nr: &mut usize,
        offset: &mut usize,
    ) -> Result<bool, SystemError> {
        let head = self.commit_head;
        let tail = self.read_tail & (NTTY_BUFSIZE - 1);

        // 计算出可读的字符数
        let mut n = (NTTY_BUFSIZE - tail).min(head - self.read_tail);
        n = n.min(*nr);

        if n > 0 {
            // 拷贝数据
            self.ntty_copy(&mut dst[*offset..], tail, &mut n)?;
            // todo:审计？
            self.read_tail += n;

            // 是否只读取了eof
            let eof =
                n == 1 && self.read_buf[tail] == termios.control_characters[ControlCharIndex::VEOF];

            if termios.local_mode.contains(LocalMode::EXTPROC)
                && self.icanon
                && eof
                && head == self.read_tail
            {
                return Ok(false);
            }

            *nr -= n;
            *offset += n;

            return Ok(head != self.read_tail);
        }

        Ok(false)
    }

    /// ## 用于处理带有 OPOST（Output Post-processing）标志的输出块的函数
    /// OPOST 是 POSIX 终端驱动器标志之一，用于指定在写入终端设备之前对输出数据进行一些后期处理。
    pub fn process_output_block(
        &mut self,
        core: &TtyCoreData,
        termios: RwLockReadGuard<Termios>,
        buf: &[u8],
        nr: usize,
    ) -> Result<usize, SystemError> {
        let mut nr = nr;
        let tty = self.tty.upgrade().unwrap();
        let space = tty.write_room(tty.core());

        // 如果读取数量大于了可用空间，则取最小的为真正的写入数量
        if nr > space {
            nr = space
        }

        let mut cnt = 0;
        for (i, c) in buf.iter().enumerate().take(nr) {
            cnt = i;
            let c = *c;
            if c as usize == 8 {
                // 表示退格
                if self.cursor_column > 0 {
                    self.cursor_column -= 1;
                }
                continue;
            }
            match c as char {
                '\n' => {
                    if termios.output_mode.contains(OutputMode::ONLRET) {
                        // 将回车映射为\n，即将\n换为回车
                        self.cursor_column = 0;
                    }
                    if termios.output_mode.contains(OutputMode::ONLCR) {
                        // 输出时将\n换为\r\n
                        break;
                    }

                    self.canon_cursor_column = self.cursor_column;
                }
                '\r' => {
                    if termios.output_mode.contains(OutputMode::ONOCR) && self.cursor_column == 0 {
                        // 光标已经在第0列，则不输出回车符
                        break;
                    }

                    if termios.output_mode.contains(OutputMode::OCRNL) {
                        break;
                    }
                    self.cursor_column = 0;
                    self.canon_cursor_column = 0;
                }
                '\t' => {
                    break;
                }
                _ => {
                    // 判断是否为控制字符
                    if !(c as char).is_control() {
                        if termios.output_mode.contains(OutputMode::OLCUC) {
                            break;
                        }

                        // 判断是否为utf8模式下的连续字符
                        if !(termios.input_mode.contains(InputMode::IUTF8)
                            && (c as usize) & 0xc0 == 0x80)
                        {
                            self.cursor_column += 1;
                        }
                    }
                }
            }
        }

        drop(termios);
        return tty.write(core, buf, cnt);
    }

    /// ## 处理回显
    pub fn process_echoes(&mut self, tty: Arc<TtyCore>) {
        if self.echo_mark == self.echo_tail {
            return;
        }
        self.echo_commit = self.echo_mark;
        let echoed = self.echoes(tty.clone());

        if echoed.is_ok() && echoed.unwrap() > 0 {
            tty.flush_chars(tty.core());
        }
    }

    #[inline(never)]
    pub fn echoes(&mut self, tty: Arc<TtyCore>) -> Result<usize, SystemError> {
        let mut space = tty.write_room(tty.core());
        let ospace = space;
        let termios = tty.core().termios();
        let core = tty.core();
        let mut tail = self.echo_tail;

        while ntty_buf_mask(self.echo_commit) != ntty_buf_mask(tail) {
            let c = self.echo_buf[ntty_buf_mask(tail)];

            if EchoOperation::from_u8(c) == EchoOperation::Start {
                if ntty_buf_mask(self.echo_commit) == ntty_buf_mask(tail + 1) {
                    self.echo_tail = tail;
                    return Ok(ospace - space);
                }

                // 获取到start，之后取第一个作为op
                let op = EchoOperation::from_u8(self.echo_buf[ntty_buf_mask(tail + 1)]);

                match op {
                    EchoOperation::Start => {
                        if space == 0 {
                            break;
                        }

                        if tty
                            .put_char(tty.core(), EchoOperation::Start.to_u8())
                            .is_err()
                        {
                            tty.write(core, &[EchoOperation::Start.to_u8()], 1)?;
                        }

                        self.cursor_column += 1;
                        space -= 1;
                        tail += 2;
                    }
                    EchoOperation::MoveBackCol => {
                        if self.cursor_column > 0 {
                            self.cursor_column -= 1;
                        }
                        tail += 2;
                    }
                    EchoOperation::SetCanonCol => {
                        self.canon_cursor_column = self.cursor_column;
                        tail += 2;
                    }
                    EchoOperation::EraseTab => {
                        if ntty_buf_mask(self.echo_commit) == ntty_buf_mask(tail + 2) {
                            self.echo_tail = tail;
                            return Ok(ospace - space);
                        }

                        // 要擦除的制表符所占用的列数
                        let mut char_num = self.echo_buf[ntty_buf_mask(tail + 2)] as usize;

                        /*
                           如果 num_chars 的最高位（0x80）未设置，
                           表示这是从输入的起始位置而不是从先前的制表符开始计算的列数。
                           在这种情况下，将 num_chars 与 ldata->canon_column 相加，否则，列数就是正常的制表符列数。
                        */
                        if char_num & 0x80 == 0 {
                            char_num += self.canon_cursor_column as usize;
                        }

                        // 计算要回退的列数，即制表符宽度减去实际占用的列数
                        let mut num_bs = 8 - (char_num & 7);
                        if num_bs > space {
                            // 表示左边没有足够空间回退
                            break;
                        }

                        space -= num_bs;
                        while num_bs != 0 {
                            num_bs -= 1;
                            // 8 => '\b'
                            if tty.put_char(tty.core(), 8).is_err() {
                                tty.write(core, &[8], 1)?;
                            }

                            if self.cursor_column > 0 {
                                self.cursor_column -= 1;
                            }
                        }

                        // 已经读取了 tail tail+1 tail+2,所以这里偏移加3
                        tail += 3;
                    }
                    EchoOperation::Undefined(ch) => {
                        match ch {
                            8 => {
                                if tty.put_char(tty.core(), 8).is_err() {
                                    tty.write(core, &[8], 1)?;
                                }
                                if tty.put_char(tty.core(), b' ').is_err() {
                                    tty.write(core, b" ", 1)?;
                                }
                                self.cursor_column -= 1;
                                space -= 1;
                                tail += 1;
                            }
                            _ => {
                                // 不是特殊字节码，则表示控制字符 例如 ^C
                                if space < 2 {
                                    break;
                                }

                                if tty.put_char(tty.core(), b'^').is_err() {
                                    tty.write(core, b"^", 1)?;
                                }

                                if tty.put_char(tty.core(), ch ^ 0o100).is_err() {
                                    tty.write(core, &[ch ^ 0o100], 1)?;
                                }

                                self.cursor_column += 2;
                                space -= 2;
                                tail += 2;
                            }
                        }
                    }
                }
            } else {
                if termios.output_mode.contains(OutputMode::OPOST) {
                    let ret = self.do_output_char(tty.clone(), c, space);

                    if ret.is_err() {
                        break;
                    }
                    space -= ret.unwrap();
                } else {
                    if space == 0 {
                        break;
                    }

                    if tty.put_char(tty.core(), c).is_err() {
                        tty.write(core, &[c], 1)?;
                    }
                    space -= 1;
                }
                tail += 1;
            }
        }

        // 如果回显缓冲区接近满（在下一次提交之前可能会发生回显溢出的情况），则丢弃足够的尾部数据以防止随后的溢出。
        while self.echo_commit > tail && self.echo_commit - tail >= ECHO_DISCARD_WATERMARK {
            if self.echo_buf[ntty_buf_mask(tail)] == EchoOperation::Start.to_u8() {
                if self.echo_buf[ntty_buf_mask(tail + 1)] == EchoOperation::EraseTab.to_u8() {
                    tail += 3;
                } else {
                    tail += 2;
                }
            } else {
                tail += 1;
            }
        }

        self.echo_tail = tail;
        return Ok(ospace - space);
    }

    /// ## 处理输出字符（带有 OPOST 处理）
    pub fn process_output(&mut self, tty: Arc<TtyCore>, c: u8) -> bool {
        let space = tty.write_room(tty.core());

        if self.do_output_char(tty, c, space).is_err() {
            return false;
        }

        true
    }

    // ## 设置带有 OPOST 处理的tty输出一个字符
    pub fn do_output_char(
        &mut self,
        tty: Arc<TtyCore>,
        c: u8,
        space: usize,
    ) -> Result<usize, SystemError> {
        if space == 0 {
            return Err(SystemError::ENOBUFS);
        }

        let termios = tty.core().termios();
        let core = tty.core();
        let mut c = c;
        if c as usize == 8 {
            // 表示退格
            if self.cursor_column > 0 {
                self.cursor_column -= 1;
            }
            if tty.put_char(tty.core(), c).is_err() {
                tty.write(core, &[c], 1)?;
            }
            return Ok(1);
        }
        match c as char {
            '\n' => {
                if termios.output_mode.contains(OutputMode::ONLRET) {
                    // 回车符
                    self.cursor_column = 0;
                }
                if termios.output_mode.contains(OutputMode::ONLCR) {
                    // 映射为“\r\n”
                    if space < 2 {
                        return Err(SystemError::ENOBUFS);
                    }
                    self.cursor_column = 0;
                    self.canon_cursor_column = 0;

                    // 通过驱动写入
                    tty.write(core, "\r\n".as_bytes(), 2)?;
                    return Ok(2);
                }

                self.canon_cursor_column = self.cursor_column;
            }
            '\r' => {
                if termios.output_mode.contains(OutputMode::ONOCR) && self.cursor_column == 0 {
                    // 光标已经在第0列，则不输出回车符
                    return Ok(0);
                }

                if termios.output_mode.contains(OutputMode::OCRNL) {
                    // 输出的\r映射为\n
                    c = b'\n';
                    if termios.output_mode.contains(OutputMode::ONLRET) {
                        // \r映射为\n,但是保留\r特性
                        self.cursor_column = 0;
                        self.canon_cursor_column = 0;
                    }
                } else {
                    self.cursor_column = 0;
                    self.canon_cursor_column = 0;
                }
            }
            '\t' => {
                // 计算输出一个\t需要的空间
                let spaces = 8 - (self.cursor_column & 7) as usize;
                if termios.output_mode.contains(OutputMode::TABDLY)
                    && OutputMode::TABDLY.bits() == OutputMode::XTABS.bits()
                {
                    // 配置的tab选项是真正输出空格到驱动
                    if space < spaces {
                        // 空间不够
                        return Err(SystemError::ENOBUFS);
                    }
                    self.cursor_column += spaces as u32;
                    // 写入sapces个空格
                    tty.write(core, "        ".as_bytes(), spaces)?;
                    return Ok(spaces);
                }
                self.cursor_column += spaces as u32;
            }
            _ => {
                // 判断是否为控制字符
                if !(c as char).is_control() {
                    if termios.output_mode.contains(OutputMode::OLCUC) {
                        c = c.to_ascii_uppercase();
                    }

                    // 判断是否为utf8模式下的连续字符
                    if !(termios.input_mode.contains(InputMode::IUTF8)
                        && (c as usize) & 0xc0 == 0x80)
                    {
                        self.cursor_column += 1;
                    }
                }
            }
        }

        if tty.put_char(tty.core(), c).is_err() {
            tty.write(core, &[c], 1)?;
        }
        Ok(1)
    }

    fn packet_mode_flush(&self, tty: &TtyCoreData) {
        let link = tty.link().unwrap();
        if link.core().contorl_info_irqsave().packet {
            tty.contorl_info_irqsave()
                .pktstatus
                .insert(TtyPacketStatus::TIOCPKT_FLUSHREAD);

            link.core().read_wq().wakeup_all();
        }
    }
}

impl TtyLineDiscipline for NTtyLinediscipline {
    fn open(&self, tty: Arc<TtyCore>) -> Result<(), system_error::SystemError> {
        // 反向绑定tty到disc
        self.disc_data().tty = Arc::downgrade(&tty);
        // 特定的tty设备在这里可能需要取消端口节流
        return self.set_termios(tty, None);
    }

    fn close(&self, _tty: Arc<TtyCore>) -> Result<(), system_error::SystemError> {
        todo!()
    }

    /// ## 重置缓冲区的基本信息
    fn flush_buffer(&self, tty: Arc<TtyCore>) -> Result<(), system_error::SystemError> {
        let core = tty.core();
        let _ = core.termios();
        let mut ldata = self.disc_data();
        ldata.read_head = 0;
        ldata.canon_head = 0;
        ldata.read_tail = 0;
        ldata.commit_head = 0;
        ldata.line_start = 0;
        ldata.erasing = false;
        ldata.read_flags.set_all(false);
        ldata.pushing = false;
        ldata.lookahead_count = 0;

        // todo: kick worker?
        // packet mode?
        if core.link().is_some() {
            ldata.packet_mode_flush(core);
        }

        Ok(())
    }

    #[inline(never)]
    fn read(
        &self,
        tty: Arc<TtyCore>,
        buf: &mut [u8],
        len: usize,
        cookie: &mut bool,
        _offset: usize,
        mode: FileMode,
    ) -> Result<usize, system_error::SystemError> {
        let mut ldata;
        if mode.contains(FileMode::O_NONBLOCK) {
            let ret = self.disc_data_try_lock();
            if ret.is_err() {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            ldata = ret.unwrap();
        } else {
            ldata = self.disc_data();
        }
        let core = tty.core();
        let termios = core.termios();
        let mut nr = len;

        let mut offset = 0;

        // 表示接着读
        if *cookie {
            // 规范且非拓展模式
            if ldata.icanon && !termios.local_mode.contains(LocalMode::EXTPROC) {
                // 跳过EOF字符
                if len == 0 {
                    ldata.canon_skip_eof();
                } else if ldata.canon_copy_from_read_buf(buf, &mut nr, &mut offset)? {
                    return Ok(len - nr);
                }
            } else if ldata.copy_from_read_buf(termios, buf, &mut nr, &mut offset)? {
                return Ok(len - nr);
            }

            // 没有数据可读

            // todo: kick worker? or 关闭节流？

            *cookie = false;
            return Ok(len - nr);
        }

        drop(termios);

        TtyJobCtrlManager::tty_check_change(tty.clone(), Signal::SIGTTIN)?;

        let mut minimum: usize = 0;
        if !ldata.icanon {
            let core = tty.core();
            let termios = core.termios();
            minimum = termios.control_characters[ControlCharIndex::VMIN] as usize;
            if minimum == 0 {
                minimum = 1;
            }
        }

        let packet = core.contorl_info_irqsave().packet;
        let mut ret: Result<usize, SystemError> = Ok(0);
        // 记录读取前 的tail
        let tail = ldata.read_tail;
        drop(ldata);
        while nr != 0 {
            // todo: 处理packet模式
            if packet {
                let link = core.link().unwrap();
                let link = link.core();
                let mut ctrl = link.contorl_info_irqsave();
                if !ctrl.pktstatus.is_empty() {
                    if offset != 0 {
                        break;
                    }
                    let cs = ctrl.pktstatus;
                    ctrl.pktstatus = TtyPacketStatus::empty();

                    buf[offset] = cs.bits();
                    offset += 1;
                    // nr -= 1;
                    break;
                }
            }

            let mut ldata = self.disc_data();

            let core = tty.core();
            if !ldata.input_available(core.termios(), false) {
                if core.flags().contains(TtyFlag::OTHER_CLOSED) {
                    ret = Err(SystemError::EIO);
                    break;
                }

                if core.flags().contains(TtyFlag::HUPPED) || core.flags().contains(TtyFlag::HUPPING)
                {
                    break;
                }

                if mode.contains(FileMode::O_NONBLOCK)
                    || core.flags().contains(TtyFlag::LDISC_CHANGING)
                {
                    ret = Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    break;
                }

                if ProcessManager::current_pcb().has_pending_signal_fast() {
                    ProcessManager::current_pcb()
                        .flags()
                        .insert(ProcessFlags::HAS_PENDING_SIGNAL);

                    ret = Err(SystemError::ERESTARTSYS);
                    break;
                }

                // 休眠一段时间
                // 获取到termios读锁，避免termios被更改导致行为异常
                // let termios = core.termios_preempt_enable();
                // let helper = WakeUpHelper::new(ProcessManager::current_pcb());
                // let wakeup_helper = Timer::new(helper, timeout);
                // wakeup_helper.activate();
                // drop(termios);
                drop(ldata);
                core.read_wq()
                    .sleep((EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as u64);
                continue;
            }

            if ldata.icanon && !core.termios().local_mode.contains(LocalMode::EXTPROC) {
                if ldata.canon_copy_from_read_buf(buf, &mut nr, &mut offset)? {
                    *cookie = true;
                    offset += len - nr;
                    return Ok(offset);
                }
            } else {
                // 非标准模式
                // todo: 处理packet模式
                if packet && offset == 0 {
                    buf[offset] = TtyPacketStatus::TIOCPKT_DATA.bits();
                    offset += 1;
                    nr -= 1;
                }
                // 拷贝数据
                if ldata.copy_from_read_buf(core.termios(), buf, &mut nr, &mut offset)?
                    && offset >= minimum
                {
                    *cookie = true;
                    return Ok(offset);
                }
            }

            if offset >= minimum {
                break;
            }
        }
        let ldata = self.disc_data();
        if tail != ldata.read_tail {
            // todo: kick worker?
        }

        if offset > 0 {
            return Ok(offset);
        }

        ret
    }

    #[inline(never)]
    fn write(
        &self,
        tty: Arc<TtyCore>,
        buf: &[u8],
        len: usize,
        mode: FileMode,
    ) -> Result<usize, system_error::SystemError> {
        let mut nr = len;
        let mut ldata = self.disc_data();
        let pcb = ProcessManager::current_pcb();
        let binding = tty.clone();
        let core = binding.core();
        let termios = *core.termios();
        if termios.local_mode.contains(LocalMode::TOSTOP) {
            TtyJobCtrlManager::tty_check_change(tty.clone(), Signal::SIGTTOU)?;
        }

        ldata.process_echoes(tty.clone());
        // drop(ldata);
        let mut offset = 0;
        loop {
            if pcb.has_pending_signal_fast() {
                ProcessManager::current_pcb()
                    .flags()
                    .insert(ProcessFlags::HAS_PENDING_SIGNAL);

                return Err(SystemError::ERESTARTSYS);
            }
            if core.flags().contains(TtyFlag::HUPPED) {
                return Err(SystemError::EIO);
            }
            if termios.output_mode.contains(OutputMode::OPOST) {
                while nr > 0 {
                    // let mut ldata = self.disc_data();
                    // 获得一次处理后的数量
                    let ret = ldata.process_output_block(core, core.termios(), &buf[offset..], nr);
                    let num = match ret {
                        Ok(num) => num,
                        Err(e) => {
                            if e == SystemError::EAGAIN_OR_EWOULDBLOCK {
                                break;
                            } else {
                                return Err(e);
                            }
                        }
                    };

                    offset += num;
                    nr -= num;

                    if nr == 0 {
                        break;
                    }

                    let c = buf[offset];
                    if !ldata.process_output(tty.clone(), c) {
                        break;
                    }
                    offset += 1;
                    nr -= 1;
                }

                tty.flush_chars(core);
            } else {
                while nr > 0 {
                    let write = tty.write(core, &buf[offset..], nr)?;
                    if write == 0 {
                        break;
                    }
                    offset += write;
                    nr -= write;
                }
            }

            if nr == 0 {
                break;
            }

            if mode.contains(FileMode::O_NONBLOCK) || core.flags().contains(TtyFlag::LDISC_CHANGING)
            {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            // 到这里表明没位置可写了
            // 休眠一段时间
            // 获取到termios读锁，避免termios被更改导致行为异常
            core.write_wq()
                .sleep(EPollEventType::EPOLLOUT.bits() as u64);
        }

        Ok(offset)
    }

    fn ioctl(
        &self,
        tty: Arc<TtyCore>,
        cmd: u32,
        arg: usize,
    ) -> Result<usize, system_error::SystemError> {
        match cmd {
            TtyIoctlCmd::TIOCOUTQ => {
                let mut user_writer = UserBufferWriter::new(
                    VirtAddr::new(arg).as_ptr::<i32>(),
                    core::mem::size_of::<i32>(),
                    true,
                )?;

                let count = tty.chars_in_buffer();
                user_writer.copy_one_to_user::<i32>(&(count as i32), 0)?;
                return Ok(0);
            }
            TtyIoctlCmd::FIONREAD => {
                let ldata = self.disc_data();
                let termios = tty.core().termios();
                let retval;
                if termios.local_mode.contains(LocalMode::ICANON)
                    && !termios.local_mode.contains(LocalMode::EXTPROC)
                {
                    if ldata.canon_head == ldata.read_tail {
                        retval = 0;
                    } else {
                        let head = ldata.canon_head;
                        let mut tail = ldata.read_tail;
                        let mut nr = head - tail;

                        while ntty_buf_mask(head) != ntty_buf_mask(tail) {
                            if ldata.read_flags.get(ntty_buf_mask(tail)).unwrap()
                                && ldata.read_buf[ntty_buf_mask(tail)]
                                    == ControlCharIndex::DISABLE_CHAR
                            {
                                nr -= 1;
                            }
                            tail += 1;
                        }

                        retval = nr;
                    }
                } else {
                    retval = ldata.read_cnt();
                }

                let mut user_writer = UserBufferWriter::new(
                    VirtAddr::new(arg).as_ptr::<i32>(),
                    core::mem::size_of::<i32>(),
                    true,
                )?;

                user_writer.copy_one_to_user::<i32>(&(retval as i32), 0)?;
                return Ok(0);
            }
            _ => {
                return self.ioctl_helper(tty, cmd, arg);
            }
        }
    }

    #[inline(never)]
    fn set_termios(
        &self,
        tty: Arc<TtyCore>,
        old: Option<crate::driver::tty::termios::Termios>,
    ) -> Result<(), system_error::SystemError> {
        let core = tty.core();
        let termios = core.termios();
        let mut ldata = self.disc_data();
        let contorl_chars = termios.control_characters;

        // 第一次设置或者规范模式 (ICANON) 或者扩展处理 (EXTPROC) 标志发生变化
        let mut spec_mode_changed = false;
        if let Some(old) = old {
            let local_mode = old.local_mode.bitxor(termios.local_mode);
            spec_mode_changed =
                local_mode.contains(LocalMode::ICANON) || local_mode.contains(LocalMode::EXTPROC);
        }
        if old.is_none() || spec_mode_changed {
            // 重置read_flags
            ldata.read_flags.set_all(false);

            ldata.line_start = ldata.read_tail;

            // 不是规范模式或者有可读数据
            if !termios.local_mode.contains(LocalMode::ICANON) || ldata.read_cnt() != 0 {
                ldata.canon_head = ldata.read_tail;
                ldata.pushing = false;
            } else {
                let read_head = ldata.read_head;
                ldata
                    .read_flags
                    .set((read_head - 1) & (NTTY_BUFSIZE - 1), true);
                ldata.canon_head = ldata.read_head;
                ldata.pushing = true;
            }
            ldata.commit_head = ldata.read_head;
            ldata.erasing = false;
            ldata.lnext = false;
        }

        // 设置模式
        ldata.icanon = termios.local_mode.contains(LocalMode::ICANON);

        // 设置回显
        if termios.local_mode.contains(LocalMode::ECHO) {
            ldata.echo = true;
        }

        if termios.input_mode.contains(InputMode::ISTRIP)
            || termios.input_mode.contains(InputMode::IUCLC)
            || termios.input_mode.contains(InputMode::IGNCR)
            || termios.input_mode.contains(InputMode::IXON)
            || termios.local_mode.contains(LocalMode::ISIG)
            || termios.local_mode.contains(LocalMode::ECHO)
            || termios.input_mode.contains(InputMode::PARMRK)
        {
            // 非原模式

            ldata.char_map.set_all(false);

            // 忽略回车符或者将回车映射为换行符
            if termios.input_mode.contains(InputMode::IGNCR)
                || termios.input_mode.contains(InputMode::ICRNL)
            {
                ldata.char_map.set('\r' as usize, true);
            }

            // 将换行映射为回车
            if termios.input_mode.contains(InputMode::INLCR) {
                ldata.char_map.set('\n' as usize, true);
            }

            // 规范模式
            if termios.local_mode.contains(LocalMode::ICANON) {
                ldata
                    .char_map
                    .set(contorl_chars[ControlCharIndex::VERASE] as usize, true);
                ldata
                    .char_map
                    .set(contorl_chars[ControlCharIndex::VKILL] as usize, true);
                ldata
                    .char_map
                    .set(contorl_chars[ControlCharIndex::VEOF] as usize, true);
                ldata.char_map.set('\n' as usize, true);
                ldata
                    .char_map
                    .set(contorl_chars[ControlCharIndex::VEOL] as usize, true);

                if termios.local_mode.contains(LocalMode::IEXTEN) {
                    ldata
                        .char_map
                        .set(contorl_chars[ControlCharIndex::VWERASE] as usize, true);
                    ldata
                        .char_map
                        .set(contorl_chars[ControlCharIndex::VLNEXT] as usize, true);
                    ldata
                        .char_map
                        .set(contorl_chars[ControlCharIndex::VEOL2] as usize, true);
                    if termios.local_mode.contains(LocalMode::ECHO) {
                        ldata
                            .char_map
                            .set(contorl_chars[ControlCharIndex::VREPRINT] as usize, true);
                    }
                }
            }

            // 软件流控制
            if termios.input_mode.contains(InputMode::IXON) {
                ldata
                    .char_map
                    .set(contorl_chars[ControlCharIndex::VSTART] as usize, true);
                ldata
                    .char_map
                    .set(contorl_chars[ControlCharIndex::VSTOP] as usize, true);
            }

            if termios.local_mode.contains(LocalMode::ISIG) {
                ldata
                    .char_map
                    .set(contorl_chars[ControlCharIndex::VINTR] as usize, true);
                ldata
                    .char_map
                    .set(contorl_chars[ControlCharIndex::VQUIT] as usize, true);
                ldata
                    .char_map
                    .set(contorl_chars[ControlCharIndex::VSUSP] as usize, true);
            }

            ldata
                .char_map
                .set(ControlCharIndex::DISABLE_CHAR as usize, true);
            ldata.raw = false;
            ldata.real_raw = false;
        } else {
            // 原模式或real_raw
            ldata.raw = true;

            ldata.real_raw = termios.input_mode.contains(InputMode::IGNBRK)
                || (!termios.input_mode.contains(InputMode::BRKINT)
                    && !termios.input_mode.contains(InputMode::PARMRK))
                    && (termios.input_mode.contains(InputMode::IGNPAR)
                        || !termios.input_mode.contains(InputMode::INPCK))
                    && (core
                        .driver()
                        .flags()
                        .contains(TtyDriverFlag::TTY_DRIVER_REAL_RAW));
        }

        // if !termios.input_mode.contains(InputMode::IXON)
        //     && old.is_some()
        //     && old.unwrap().input_mode.contains(InputMode::IXON) && !
        // {}

        core.read_wq().wakeup_all();
        core.write_wq().wakeup_all();
        Ok(())
    }

    fn poll(&self, tty: Arc<TtyCore>) -> Result<usize, system_error::SystemError> {
        let core = tty.core();
        let ldata = self.disc_data();

        let mut event = EPollEventType::empty();
        if ldata.input_available(core.termios(), true) {
            event.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM)
        }

        if core.contorl_info_irqsave().packet {
            let link = core.link();
            if link.is_some()
                && !link
                    .unwrap()
                    .core()
                    .contorl_info_irqsave()
                    .pktstatus
                    .is_empty()
            {
                event.insert(
                    EPollEventType::EPOLLPRI
                        | EPollEventType::EPOLLIN
                        | EPollEventType::EPOLLRDNORM,
                );
            }
        }

        if core.flags().contains(TtyFlag::OTHER_CLOSED) {
            event.insert(EPollEventType::EPOLLHUP);
        }

        if core.driver().driver_funcs().chars_in_buffer() < 256
            && core.driver().driver_funcs().write_room(core) > 0
        {
            event.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
        }

        Ok(event.bits() as usize)
    }

    fn hangup(&self, _tty: Arc<TtyCore>) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn receive_buf(
        &self,
        tty: Arc<TtyCore>,
        buf: &[u8],
        flags: Option<&[u8]>,
        count: usize,
    ) -> Result<usize, SystemError> {
        let mut ldata = self.disc_data();
        ldata.receive_buf_common(tty, buf, flags, count, false)
    }

    fn receive_buf2(
        &self,
        tty: Arc<TtyCore>,
        buf: &[u8],
        flags: Option<&[u8]>,
        count: usize,
    ) -> Result<usize, SystemError> {
        let mut ldata = self.disc_data();
        ldata.receive_buf_common(tty, buf, flags, count, true)
    }
}
