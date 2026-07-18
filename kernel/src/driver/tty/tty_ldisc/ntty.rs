use alloc::{boxed::Box, vec::Vec};
use core::{intrinsics::likely, mem, ops::BitXor};

use bitmap::{static_bitmap, traits::BitMapOps, StaticBitmap};

use alloc::sync::{Arc, Weak};
use system_error::SystemError;

use crate::{
    arch::ipc::signal::Signal,
    driver::tty::{
        kthread::{retry_tty_input_producers, tty_kick_input_worker},
        pty::unix98pty::{
            pty_drain_pending_to, pty_flush_input_buffer, pty_receive_flush_input_buffer,
        },
        termios::{ControlCharIndex, InputMode, LocalMode, OutputMode, Termios},
        tty_core::{
            EchoOperation, TtyCore, TtyCoreData, TtyFlag, TtyIoctlCmd, TtyPacketStatus,
            TtySleepLock,
        },
        tty_driver::{TtyDriverFlag, TtyDriverSubType, TtyOperation},
        tty_job_control::TtyJobCtrlManager,
    },
    filesystem::{
        epoll::{event_poll::EventPoll, EPollEventType},
        vfs::file::FileFlags,
    },
    libs::{
        rwlock::RwLockReadGuard,
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::VirtAddr,
    process::ProcessManager,
    syscall::user_access::UserBufferWriter,
    time::Duration,
};

use super::TtyLineDiscipline;
pub const NTTY_BUFSIZE: usize = 4096;
pub const ECHO_COMMIT_WATERMARK: usize = 256;
pub const ECHO_BLOCK: usize = 256;
pub const ECHO_DISCARD_WATERMARK: usize = NTTY_BUFSIZE - (ECHO_BLOCK + 32);

fn ntty_buf_mask(idx: usize) -> usize {
    return idx & (NTTY_BUFSIZE - 1);
}

fn wake_tty_readers(tty: &Arc<TtyCore>, events: EPollEventType) {
    tty.core().read_wq().wakeup_any(events.bits() as u64);
    let _ = EventPoll::wakeup_epoll(tty.core().epitems(), events);
}

fn is_ascii_control(c: u8) -> bool {
    c < b' ' || c == 0x7f
}

fn output_mode_has_xtabs(termios: &Termios) -> bool {
    OutputMode::from_bits_truncate(termios.output_mode.bits() & OutputMode::TABDLY.bits())
        == OutputMode::XTABS
}

#[derive(Debug)]
pub struct NTtyLinediscipline {
    pub data: SpinLock<NTtyData>,
    pub(crate) output_lock: TtySleepLock,
}

#[derive(Debug, Clone)]
struct EchoStep {
    bytes: Vec<u8>,
    tail: usize,
    cursor_column: u32,
    canon_cursor_column: u32,
}

#[derive(Debug)]
struct EchoPendingStep {
    bytes: Vec<u8>,
    offset: usize,
    step: EchoStep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpostCharResult {
    Emitted,
    ConsumedWithoutOutput,
    NeedsMoreRoom,
}

#[derive(Debug, Clone, Copy)]
enum NTtyReadWait {
    NoWait,
    Forever,
    Timeout(Duration),
}

impl NTtyLinediscipline {
    #[inline]
    pub fn disc_data(&self) -> SpinLockGuard<'_, NTtyData> {
        self.data.lock_irqsave()
    }

    #[inline]
    pub fn disc_data_try_lock(&self) -> Result<SpinLockGuard<'_, NTtyData>, SystemError> {
        self.data.try_lock_irqsave()
    }

    fn ioctl_helper(&self, tty: Arc<TtyCore>, cmd: u32, arg: usize) -> Result<usize, SystemError> {
        match cmd {
            TtyIoctlCmd::TCXONC => {
                todo!()
            }
            TtyIoctlCmd::TCFLSH => TtyCore::tty_perform_flush(tty, arg),
            _ => {
                return TtyCore::tty_mode_ioctl(tty.clone(), cmd, arg);
            }
        }
    }

    fn drain_opost_pending(&self, tty: &TtyCore) -> Result<bool, SystemError> {
        let core = tty.core();
        loop {
            let pending = self.disc_data().opost_pending_bytes().to_vec();
            if pending.is_empty() {
                return Ok(true);
            }

            let written = tty.write(core, &pending, pending.len())?;
            if written == 0 {
                core.flags_write().insert(TtyFlag::DO_WRITE_WAKEUP);
                return Ok(false);
            }

            self.disc_data().advance_opost_pending(written);
            tty.flush_chars(core);
        }
    }

    fn drain_echoes(&self, tty: &TtyCore) -> Result<(), SystemError> {
        let core = tty.core();
        loop {
            while let Some(bytes) = { self.disc_data().echo_pending_bytes() } {
                let written = tty.write(core, &bytes, bytes.len())?;
                if written == 0 {
                    core.flags_write().insert(TtyFlag::DO_WRITE_WAKEUP);
                    return Ok(());
                }
                {
                    let mut guard = self.disc_data();
                    guard.advance_echo_pending(written);
                }
                tty.flush_chars(core);
            }

            let termios = *core.termios();
            let space = tty.write_room(core);
            let step = {
                let guard = self.disc_data();
                guard.next_echo_step(&termios, space)
            };

            let Some(step) = step else {
                break;
            };

            if !step.bytes.is_empty() {
                let mut sent = 0;
                while sent < step.bytes.len() {
                    let written = tty.write(core, &step.bytes[sent..], step.bytes.len() - sent)?;
                    if written == 0 {
                        if sent != 0 {
                            let mut guard = self.disc_data();
                            guard.set_echo_pending_step(step, sent);
                        }
                        core.flags_write().insert(TtyFlag::DO_WRITE_WAKEUP);
                        return Ok(());
                    }
                    sent += written;
                }
                tty.flush_chars(core);
            }

            let mut guard = self.disc_data();
            guard.apply_echo_step(step);
        }

        if self.disc_data().has_output_wakeup_pending() {
            core.flags_write().insert(TtyFlag::DO_WRITE_WAKEUP);
        } else {
            core.flags_write().remove(TtyFlag::DO_WRITE_WAKEUP);
        }
        Ok(())
    }

    fn packet_status_pending(core: &TtyCoreData, packet: bool) -> bool {
        if !packet {
            return false;
        }
        let Some(link) = core.link() else {
            return false;
        };
        let pending = !link.core().contorl_info_irqsave().pktstatus.is_empty();
        pending
    }

    fn check_pty_unthrottle_after_read(tty: &Arc<TtyCore>) {
        let _ = pty_drain_pending_to(tty.clone());
        match tty.core().driver().tty_driver_sub_type() {
            TtyDriverSubType::PtyMaster | TtyDriverSubType::PtySlave => {
                if let Some(peer) = tty.core().link() {
                    peer.tty_wakeup();
                }
            }
            _ => {}
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
    /// OPOST 处理后尚未被底层 driver 接收的输出字节。
    opost_pending: Vec<u8>,
    opost_pending_offset: usize,
    echo_pending_step: Option<EchoPendingStep>,
    /// 回显缓冲区的尾指针
    echo_tail: usize,

    /// 写者与读者共享
    read_buf: Box<[u8; NTTY_BUFSIZE]>,
    echo_buf: Box<[u8; NTTY_BUFSIZE]>,

    read_flags: static_bitmap!(NTTY_BUFSIZE),
    char_map: static_bitmap!(256),

    deferred_tty_wakeup: bool,
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
            opost_pending: Vec::with_capacity(NTTY_BUFSIZE),
            opost_pending_offset: 0,
            echo_pending_step: None,
            echo_tail: 0,
            read_buf: vec![0; NTTY_BUFSIZE].into_boxed_slice().try_into().unwrap(),
            echo_buf: vec![0; NTTY_BUFSIZE].into_boxed_slice().try_into().unwrap(),
            read_flags: StaticBitmap::new(),
            char_map: StaticBitmap::new(),
            deferred_tty_wakeup: false,
            tty: Weak::default(),
            no_room: false,
        }
    }

    fn take_deferred_tty_wakeup(&mut self) -> bool {
        mem::take(&mut self.deferred_tty_wakeup)
    }

    fn opost_pending_bytes(&self) -> &[u8] {
        if self.opost_pending_offset >= self.opost_pending.len() {
            &[]
        } else {
            &self.opost_pending[self.opost_pending_offset..]
        }
    }

    fn advance_opost_pending(&mut self, count: usize) {
        self.opost_pending_offset += count;
        if self.opost_pending_offset >= self.opost_pending.len() {
            self.opost_pending.clear();
            self.opost_pending_offset = 0;
        }
    }

    fn echo_pending_bytes(&self) -> Option<Vec<u8>> {
        let pending = self.echo_pending_step.as_ref()?;
        if pending.offset >= pending.bytes.len() {
            None
        } else {
            Some(pending.bytes[pending.offset..].to_vec())
        }
    }

    fn advance_echo_pending(&mut self, count: usize) {
        let Some(pending) = self.echo_pending_step.as_mut() else {
            return;
        };
        pending.offset += count;
        if pending.offset >= pending.bytes.len() {
            let pending = self.echo_pending_step.take().unwrap();
            self.apply_echo_step(pending.step);
        }
    }

    fn set_echo_pending_step(&mut self, step: EchoStep, offset: usize) {
        self.echo_pending_step = Some(EchoPendingStep {
            bytes: step.bytes.clone(),
            offset,
            step,
        });
    }

    fn has_echo_output_pending(&self) -> bool {
        self.echo_pending_step.is_some()
            || ntty_buf_mask(self.echo_commit) != ntty_buf_mask(self.echo_tail)
    }

    fn discard_output_state(&mut self) {
        self.opost_pending.clear();
        self.opost_pending_offset = 0;
        self.echo_pending_step = None;
        self.echo_tail = self.echo_head;
        self.echo_mark = self.echo_head;
        self.echo_commit = self.echo_head;
    }

    fn has_output_wakeup_pending(&self) -> bool {
        !self.opost_pending_bytes().is_empty() || self.has_echo_output_pending()
    }

    fn opost_char_space(&self, termios: &Termios, c: u8) -> usize {
        if c as usize == 8 {
            return 1;
        }

        match c as char {
            '\n' if termios.output_mode.contains(OutputMode::ONLCR) => 2,
            '\r' if termios.output_mode.contains(OutputMode::ONOCR) && self.cursor_column == 0 => 0,
            '\t' if output_mode_has_xtabs(termios) => (8 - (self.cursor_column & 7)) as usize,
            _ => 1,
        }
    }

    fn opost_progress_possible(
        &self,
        termios: &Termios,
        next_input: Option<u8>,
        write_room: usize,
    ) -> bool {
        if !self.opost_pending_bytes().is_empty() {
            return write_room > 0;
        }

        let Some(c) = next_input else {
            return false;
        };
        if write_room == 0 {
            return false;
        }
        self.opost_char_space(termios, c) <= write_room
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
        let mut n;
        let mut offset = 0;
        let mut recved = 0;
        loop {
            let tail = self.read_tail;

            let mut room = NTTY_BUFSIZE as isize - (self.read_head - tail) as isize;
            if termios.input_mode.contains(InputMode::PARMRK) {
                room = if room > 0 { (room + 2) / 3 } else { room };
            }

            room -= 1;
            if room <= 0 {
                // 可能溢出
                let overflow = self.icanon && self.canon_head == tail;
                if overflow && room < 0 {
                    self.read_head -= 1;
                }
                self.no_room = flow && !overflow;
                room = if overflow { 1 } else { 0 };
            }

            n = count.min(room as usize);
            if n == 0 {
                break;
            }

            if let Some(flags) = flags {
                self.receive_buf(
                    tty.clone(),
                    &*termios,
                    &buf[offset..],
                    Some(&flags[offset..]),
                    n,
                );
            } else {
                self.receive_buf(tty.clone(), &*termios, &buf[offset..], flags, n);
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
        termios: &Termios,
        buf: &[u8],
        flags: Option<&[u8]>,
        count: usize,
    ) {
        let preops = termios.input_mode.contains(InputMode::ISTRIP)
            || termios.input_mode.contains(InputMode::IUCLC)
            || termios.local_mode.contains(LocalMode::IEXTEN);
        let extproc = termios.local_mode.contains(LocalMode::EXTPROC);

        let look_ahead = self.lookahead_count.min(count);
        if self.real_raw {
            self.receive_buf_real_raw(buf, count);
        } else if self.raw || (extproc && !preops) {
            self.receive_buf_raw(buf, flags, count);
        } else if tty.core().is_closing() && !extproc {
            todo!()
        } else {
            if look_ahead > 0 {
                self.receive_buf_standard(tty.clone(), termios, buf, flags, look_ahead, true);
            }

            if count > look_ahead {
                let remaining = &buf[look_ahead..];
                let remaining_flags = flags.map(|f| &f[look_ahead..]);
                self.receive_buf_standard(
                    tty.clone(),
                    termios,
                    remaining,
                    remaining_flags,
                    count - look_ahead,
                    false,
                );
            }

            // 刷新echo
            self.flush_echoes(termios);

            tty.flush_chars(tty.core());
        }

        self.lookahead_count -= look_ahead;

        if self.icanon && !extproc {
            return;
        }

        self.commit_head = self.read_head;

        if self.read_cnt() > 0 {
            wake_tty_readers(&tty, EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
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
            if let Some(flags_slice) = flags.as_ref() {
                flag = flags_slice[f_offset];
                f_offset += 1;
            }

            if likely(flag == 1) {
                self.read_buf[ntty_buf_mask(self.read_head)] = buf[c_offset];
                c_offset += 1;
                self.read_head += 1;
            } else {
                todo!()
            }

            count -= 1;
        }
    }

    pub fn flush_echoes(&mut self, termios: &Termios) {
        if !termios.local_mode.contains(LocalMode::ECHO)
            && !termios.local_mode.contains(LocalMode::ECHONL)
            || self.echo_commit == self.echo_head
        {
            return;
        }

        self.echo_commit = self.echo_head;
    }

    pub fn receive_buf_standard(
        &mut self,
        tty: Arc<TtyCore>,
        termios: &Termios,
        buf: &[u8],
        flags: Option<&[u8]>,
        mut count: usize,
        lookahead_done: bool,
    ) {
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
                    self.receive_char(c, tty.clone(), termios)
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

            if ((c as usize) < self.char_map.len()) && self.char_map.get(c as usize).unwrap() {
                // 特殊字符
                self.receive_special_char(c, tty.clone(), termios, lookahead_done);
            } else {
                self.receive_char(c, tty.clone(), termios);
            }

            count -= 1;
        }
    }

    #[inline(never)]
    pub fn receive_special_char(
        &mut self,
        mut c: u8,
        tty: Arc<TtyCore>,
        termios: &Termios,
        lookahead_done: bool,
    ) {
        let is_flow_ctrl = self.is_flow_ctrl_char(tty.clone(), termios, c, lookahead_done);

        // 启用软件流控，并且该字符已经当做软件流控字符处理
        if termios.input_mode.contains(InputMode::IXON) && is_flow_ctrl {
            return;
        }

        if termios.local_mode.contains(LocalMode::ISIG) {
            if c == termios.control_characters[ControlCharIndex::VINTR] {
                self.recv_sig_char(tty.clone(), termios, Signal::SIGINT, c);
                return;
            }

            if c == termios.control_characters[ControlCharIndex::VQUIT] {
                self.recv_sig_char(tty.clone(), termios, Signal::SIGQUIT, c);
                return;
            }

            if c == termios.control_characters[ControlCharIndex::VSUSP] {
                self.recv_sig_char(tty.clone(), termios, Signal::SIGTSTP, c);
                return;
            }
        }

        if termios.input_mode.contains(InputMode::IXON)
            && termios.input_mode.contains(InputMode::IXANY)
        {
            let started = tty.tty_start_without_wakeup();
            self.deferred_tty_wakeup |= started;
            if started {
                self.process_echoes(tty.clone());
            }
        }

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
                self.eraser(c, termios);
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
                self.echo_char(c, termios);
                self.echo_char_raw(b'\n');
                while ntty_buf_mask(tail) != ntty_buf_mask(self.read_head) {
                    self.echo_char(self.read_buf[ntty_buf_mask(tail)], termios);
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
                wake_tty_readers(&tty, EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
                return;
            }

            if c == termios.control_characters[ControlCharIndex::VEOF] {
                c = ControlCharIndex::DISABLE_CHAR;

                self.read_flags.set(ntty_buf_mask(self.read_head), true);
                self.read_buf[ntty_buf_mask(self.read_head)] = c;
                self.read_head += 1;
                self.canon_head = self.read_head;
                wake_tty_readers(&tty, EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
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
                    self.echo_char(c, termios);
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
                wake_tty_readers(&tty, EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
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
                self.echo_char(c, termios);
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
    fn eraser(&mut self, mut c: u8, termios: &Termios) {
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

        let mut seen_alnums = false;
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
                if c.is_ascii_alphanumeric() || c == b'_' {
                    seen_alnums = true;
                } else if seen_alnums {
                    break;
                }
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
                        } else if is_ascii_control(c) {
                            if termios.local_mode.contains(LocalMode::ECHOCTL) {
                                num_chars += 2;
                            }
                        } else if !Self::is_continuation(c, termios) {
                            num_chars += 1;
                        }
                    }

                    self.echo_erase_tab(num_chars, after_tab);
                } else {
                    if is_ascii_control(c) && termios.local_mode.contains(LocalMode::ECHOCTL) {
                        // 8 => '\b'
                        self.echo_char_raw(8);
                        self.echo_char_raw(b' ');
                        self.echo_char_raw(8);
                    }

                    if !is_ascii_control(c) || termios.local_mode.contains(LocalMode::ECHOCTL) {
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
    fn is_continuation(c: u8, termios: &Termios) -> bool {
        return termios.input_mode.contains(InputMode::IUTF8) && (c & 0xc0) == 0x80;
    }

    /// ## 该字符是否已经当做流控字符处理
    pub fn is_flow_ctrl_char(
        &mut self,
        tty: Arc<TtyCore>,
        termios: &Termios,
        c: u8,
        lookahead_done: bool,
    ) -> bool {
        if !(termios.control_characters[ControlCharIndex::VSTART] == c
            || termios.control_characters[ControlCharIndex::VSTOP] == c)
        {
            return false;
        }

        if lookahead_done {
            return true;
        }

        if termios.control_characters[ControlCharIndex::VSTART] == c {
            self.deferred_tty_wakeup |= tty.tty_start_without_wakeup();
            self.process_echoes(tty.clone());
            return true;
        } else {
            tty.tty_stop();
            return true;
        }
    }

    /// ## 接收到信号字符时的处理
    fn recv_sig_char(&mut self, tty: Arc<TtyCore>, termios: &Termios, signal: Signal, c: u8) {
        self.input_signal(tty.clone(), termios, signal);
        if termios.input_mode.contains(InputMode::IXON) {
            self.deferred_tty_wakeup |= tty.tty_start_without_wakeup();
        }

        if termios.local_mode.contains(LocalMode::ECHO) {
            self.echo_char(c, termios);
            self.commit_echoes(tty);
        } else {
            self.process_echoes(tty);
        }
    }

    /// ## 处理输入信号
    pub fn input_signal(&mut self, tty: Arc<TtyCore>, termios: &Termios, signal: Signal) {
        // 先处理信号
        let ctrl_info = tty.core().contorl_info_irqsave();
        let pg = ctrl_info.pgid.clone();
        drop(ctrl_info);
        if let Some(pg) = pg {
            let _ = crate::ipc::kill::send_signal_to_pgid(&pg, signal);
        }

        if !termios.local_mode.contains(LocalMode::NOFLSH) {
            self.discard_output_state();

            if let Some(port) = tty.core().port() {
                if port.clear_input_from_receive() != 0 {
                    retry_tty_input_producers();
                }
            }

            let ret = tty.core().driver().driver_funcs().flush_buffer(tty.core());
            if ret != Err(SystemError::ENOSYS) {
                let _ = ret;
            }

            let _ = pty_receive_flush_input_buffer(tty.clone(), || {
                self.read_head = 0;
                self.canon_head = 0;
                self.read_tail = 0;
                self.commit_head = 0;
                self.line_start = 0;

                self.erasing = false;
                self.read_flags.set_all(false);
                self.pushing = false;
                self.lookahead_count = 0;

                if tty.core().link().is_some() {
                    self.packet_mode_flush(tty.core());
                }
            });
        }
    }

    pub fn receive_char(&mut self, c: u8, tty: Arc<TtyCore>, termios: &Termios) {
        if termios.local_mode.contains(LocalMode::ECHO) {
            if self.erasing {
                self.add_echo_byte(b'/');
                self.erasing = false;
            }

            if self.canon_head == self.read_head {
                self.add_echo_byte(EchoOperation::Start.to_u8());
                self.add_echo_byte(EchoOperation::SetCanonCol.to_u8());
            }

            self.echo_char(c, termios);
            self.commit_echoes(tty.clone());
        }

        if c == 0o377 && termios.input_mode.contains(InputMode::PARMRK) {
            self.add_read_byte(c);
        }
        self.add_read_byte(c);
    }

    pub fn echo_char(&mut self, c: u8, termios: &Termios) {
        if c == EchoOperation::Start.to_u8() {
            self.add_echo_byte(EchoOperation::Start.to_u8());
            self.add_echo_byte(EchoOperation::Start.to_u8());
        } else {
            if termios.local_mode.contains(LocalMode::ECHOCTL) && is_ascii_control(c) && c != b'\t'
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
    pub fn commit_echoes(&mut self, _tty: Arc<TtyCore>) {
        let head = self.echo_head;
        self.echo_mark = head;
        let old = self.echo_commit - self.echo_tail;

        // 需要echo的字符个数
        let nr = head - self.echo_tail;

        if nr < ECHO_COMMIT_WATERMARK || nr % ECHO_BLOCK > old % ECHO_BLOCK {
            return;
        }

        self.echo_commit = head;
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
        // 注意：next_index可能不包括起始位置，所以我们需要手动检查tail位置

        let tmp: Option<usize> = if self.read_flags.get(tail).unwrap_or(false) {
            Some(tail)
        } else {
            self.read_flags.next_index(tail)
        };
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

        // 当找到EOL时，表示一行已读取完成
        if found {
            if !self.pushing {
                self.line_start = self.read_tail;
            } else {
                self.pushing = false;
            }
            // todo: 审计？
            return Ok(false);
        }

        // 未找到EOL
        // 如果nr已被完全消耗（变为0），说明用户请求的读取长度已满足
        // 即使缓冲区中还有更多数据（包括同一行的后续数据），
        // 也应该返回false，让调用方返回已读取的字节数
        // 下次调用时会继续读取下一批数据
        return Ok(*nr > 0 && self.read_tail != canon_head);
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

    /// ## 处理回显
    pub fn process_echoes(&mut self, _tty: Arc<TtyCore>) {
        if self.echo_mark != self.echo_tail {
            self.echo_commit = self.echo_mark;
        }
    }

    fn next_echo_step(&self, termios: &Termios, space: usize) -> Option<EchoStep> {
        let mut tail = self.echo_tail;
        if ntty_buf_mask(self.echo_commit) == ntty_buf_mask(tail) {
            return None;
        }

        let mut cursor_column = self.cursor_column;
        let mut canon_cursor_column = self.canon_cursor_column;
        let mut bytes = Vec::with_capacity(8);
        let c = self.echo_buf[ntty_buf_mask(tail)];

        if EchoOperation::from_u8(c) == EchoOperation::Start {
            if ntty_buf_mask(self.echo_commit) == ntty_buf_mask(tail + 1) {
                return None;
            }

            match EchoOperation::from_u8(self.echo_buf[ntty_buf_mask(tail + 1)]) {
                EchoOperation::Start => {
                    if space < 1 {
                        return None;
                    }
                    bytes.push(EchoOperation::Start.to_u8());
                    cursor_column += 1;
                    tail += 2;
                }
                EchoOperation::MoveBackCol => {
                    cursor_column = cursor_column.saturating_sub(1);
                    tail += 2;
                }
                EchoOperation::SetCanonCol => {
                    canon_cursor_column = cursor_column;
                    tail += 2;
                }
                EchoOperation::EraseTab => {
                    if ntty_buf_mask(self.echo_commit) == ntty_buf_mask(tail + 2) {
                        return None;
                    }
                    let mut char_num = self.echo_buf[ntty_buf_mask(tail + 2)] as usize;
                    if char_num & 0x80 == 0 {
                        char_num += canon_cursor_column as usize;
                    }
                    let num_bs = 8 - (char_num & 7);
                    if num_bs > space {
                        return None;
                    }
                    bytes.resize(num_bs, 8);
                    cursor_column = cursor_column.saturating_sub(num_bs as u32);
                    tail += 3;
                }
                EchoOperation::Undefined(ch) => match ch {
                    8 => {
                        if space < 2 {
                            return None;
                        }
                        bytes.extend_from_slice(&[8, b' ']);
                        cursor_column = cursor_column.saturating_sub(1);
                        tail += 1;
                    }
                    _ => {
                        if space < 2 {
                            return None;
                        }
                        bytes.extend_from_slice(&[b'^', ch ^ 0o100]);
                        cursor_column += 2;
                        tail += 2;
                    }
                },
            }
        } else if termios.output_mode.contains(OutputMode::OPOST) {
            if Self::format_output_char(
                termios,
                c,
                &mut bytes,
                space,
                &mut cursor_column,
                &mut canon_cursor_column,
            )
            .is_err()
            {
                return None;
            }
            tail += 1;
        } else {
            if space < 1 {
                return None;
            }
            bytes.push(c);
            tail += 1;
        }

        Some(EchoStep {
            bytes,
            tail: self.echo_discard_tail(tail),
            cursor_column,
            canon_cursor_column,
        })
    }

    fn echo_discard_tail(&self, mut tail: usize) -> usize {
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
        tail
    }

    fn apply_echo_step(&mut self, step: EchoStep) {
        self.echo_tail = step.tail;
        self.cursor_column = step.cursor_column;
        self.canon_cursor_column = step.canon_cursor_column;
    }

    fn format_output_char(
        termios: &Termios,
        mut c: u8,
        out: &mut Vec<u8>,
        space: usize,
        cursor_column: &mut u32,
        canon_cursor_column: &mut u32,
    ) -> Result<usize, SystemError> {
        let used = out.len();
        if used >= space {
            return Err(SystemError::ENOBUFS);
        }

        if c as usize == 8 {
            *cursor_column = cursor_column.saturating_sub(1);
            out.push(c);
            return Ok(1);
        }

        match c as char {
            '\n' => {
                if termios.output_mode.contains(OutputMode::ONLRET) {
                    *cursor_column = 0;
                }
                if termios.output_mode.contains(OutputMode::ONLCR) {
                    if used + 2 > space {
                        return Err(SystemError::ENOBUFS);
                    }
                    *cursor_column = 0;
                    *canon_cursor_column = 0;
                    out.extend_from_slice(b"\r\n");
                    return Ok(2);
                }
                *canon_cursor_column = *cursor_column;
            }
            '\r' => {
                if termios.output_mode.contains(OutputMode::ONOCR) && *cursor_column == 0 {
                    return Ok(0);
                }

                if termios.output_mode.contains(OutputMode::OCRNL) {
                    c = b'\n';
                    if termios.output_mode.contains(OutputMode::ONLRET) {
                        *cursor_column = 0;
                        *canon_cursor_column = 0;
                    }
                } else {
                    *cursor_column = 0;
                    *canon_cursor_column = 0;
                }
            }
            '\t' => {
                let spaces = 8 - (*cursor_column & 7) as usize;
                if output_mode_has_xtabs(termios) {
                    if used + spaces > space {
                        return Err(SystemError::ENOBUFS);
                    }
                    *cursor_column += spaces as u32;
                    out.extend_from_slice(&b"        "[..spaces]);
                    return Ok(spaces);
                }
                *cursor_column += spaces as u32;
            }
            _ => {
                if !is_ascii_control(c) {
                    if termios.output_mode.contains(OutputMode::OLCUC) {
                        c = c.to_ascii_uppercase();
                    }

                    if !(termios.input_mode.contains(InputMode::IUTF8)
                        && (c as usize) & 0xc0 == 0x80)
                    {
                        *cursor_column += 1;
                    }
                }
            }
        }

        out.push(c);
        Ok(1)
    }

    fn process_output_char_to_buf(
        &mut self,
        termios: &Termios,
        mut c: u8,
        out: &mut Vec<u8>,
        space: usize,
    ) -> OpostCharResult {
        let used = out.len();
        if used >= space {
            return OpostCharResult::NeedsMoreRoom;
        }

        if c as usize == 8 {
            if self.cursor_column > 0 {
                self.cursor_column -= 1;
            }
            out.push(c);
            return OpostCharResult::Emitted;
        }

        match c as char {
            '\n' => {
                if termios.output_mode.contains(OutputMode::ONLRET) {
                    self.cursor_column = 0;
                }
                if termios.output_mode.contains(OutputMode::ONLCR) {
                    if used + 2 > space {
                        return OpostCharResult::NeedsMoreRoom;
                    }
                    self.cursor_column = 0;
                    self.canon_cursor_column = 0;
                    out.extend_from_slice(b"\r\n");
                    return OpostCharResult::Emitted;
                }
                self.canon_cursor_column = self.cursor_column;
            }
            '\r' => {
                if termios.output_mode.contains(OutputMode::ONOCR) && self.cursor_column == 0 {
                    return OpostCharResult::ConsumedWithoutOutput;
                }

                if termios.output_mode.contains(OutputMode::OCRNL) {
                    c = b'\n';
                    if termios.output_mode.contains(OutputMode::ONLRET) {
                        self.cursor_column = 0;
                        self.canon_cursor_column = 0;
                    }
                } else {
                    self.cursor_column = 0;
                    self.canon_cursor_column = 0;
                }
            }
            '\t' => {
                let spaces = 8 - (self.cursor_column & 7) as usize;
                if output_mode_has_xtabs(termios) {
                    if used + spaces > space {
                        return OpostCharResult::NeedsMoreRoom;
                    }
                    self.cursor_column += spaces as u32;
                    out.extend_from_slice(&b"        "[..spaces]);
                    return OpostCharResult::Emitted;
                }
                self.cursor_column += spaces as u32;
            }
            _ => {
                if !is_ascii_control(c) {
                    if termios.output_mode.contains(OutputMode::OLCUC) {
                        c = c.to_ascii_uppercase();
                    }

                    if !(termios.input_mode.contains(InputMode::IUTF8)
                        && (c as usize) & 0xc0 == 0x80)
                    {
                        self.cursor_column += 1;
                    }
                }
            }
        }

        out.push(c);
        OpostCharResult::Emitted
    }

    fn simple_output_block_len(&self, termios: &Termios, buf: &[u8], limit: usize) -> usize {
        let mut len = 0;
        let limit = limit.min(buf.len());
        while len < limit {
            let c = buf[len];
            match c as char {
                '\n' | '\r' | '\t' => break,
                _ => {
                    if !is_ascii_control(c) && termios.output_mode.contains(OutputMode::OLCUC) {
                        break;
                    }
                }
            }
            len += 1;
        }
        len
    }

    fn apply_simple_output_columns(&mut self, termios: &Termios, buf: &[u8]) {
        for &c in buf {
            if c as usize == 8 {
                if self.cursor_column > 0 {
                    self.cursor_column -= 1;
                }
            } else if !(is_ascii_control(c)
                || termios.input_mode.contains(InputMode::IUTF8) && (c as usize) & 0xc0 == 0x80)
            {
                self.cursor_column += 1;
            }
        }
    }

    fn packet_mode_flush(&self, tty: &TtyCoreData) {
        let link = tty.link().unwrap();
        if link.core().contorl_info_irqsave().packet {
            tty.contorl_info_irqsave()
                .pktstatus
                .insert(TtyPacketStatus::TIOCPKT_FLUSHREAD);

            link.core().read_wq().wakeup_all();
            let _ = EventPoll::wakeup_epoll(
                link.core().epitems(),
                EPollEventType::EPOLLPRI | EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
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

    fn close(&self, tty: Arc<TtyCore>) -> Result<(), system_error::SystemError> {
        self.flush_buffer(tty.clone())?;
        self.flush_output(tty)
    }

    /// ## 重置缓冲区的基本信息
    fn flush_buffer(&self, tty: Arc<TtyCore>) -> Result<(), system_error::SystemError> {
        let core = tty.core();
        if let Some(port) = core.port() {
            if port.clear_input() != 0 {
                retry_tty_input_producers();
            }
        }
        pty_flush_input_buffer(tty.clone(), || {
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
            ldata.no_room = false;

            if core.link().is_some() {
                ldata.packet_mode_flush(core);
            }
        })?;

        core.read_wq().wakeup_all();
        core.write_wq().wakeup_all();

        if let Some(link) = core.link() {
            link.core().write_wq().wakeup_all();
        }

        Ok(())
    }

    fn flush_output(&self, tty: Arc<TtyCore>) -> Result<(), system_error::SystemError> {
        let _output_guard = self.output_lock.lock();
        let mut ldata = self.disc_data();
        ldata.discard_output_state();

        let ret = tty.core().driver().driver_funcs().flush_buffer(tty.core());
        if ret != Err(SystemError::ENOSYS) {
            ret?;
        }
        tty.core().flags_write().remove(TtyFlag::DO_WRITE_WAKEUP);
        tty.core().write_wq().wakeup_all();

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
        flags: FileFlags,
    ) -> Result<usize, system_error::SystemError> {
        let mut ldata;
        if flags.contains(FileFlags::O_NONBLOCK) {
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
            let tail = ldata.read_tail;
            let is_canon = ldata.icanon && !termios.local_mode.contains(LocalMode::EXTPROC);
            // 规范且非拓展模式
            if is_canon {
                drop(termios);
                // 跳过EOF字符
                if len == 0 {
                    ldata.canon_skip_eof();
                } else {
                    let _ = ldata.canon_copy_from_read_buf(buf, &mut nr, &mut offset)?;
                }
            } else {
                let _ = ldata.copy_from_read_buf(termios, buf, &mut nr, &mut offset)?;
            }

            *cookie = false;
            let read_tail_moved = tail != ldata.read_tail;
            drop(ldata);
            if read_tail_moved {
                tty_kick_input_worker(tty.clone());
                Self::check_pty_unthrottle_after_read(&tty);
            }
            return Ok(offset);
        }

        drop(termios);

        TtyJobCtrlManager::tty_check_change(tty.clone(), Signal::SIGTTIN)?;

        let mut minimum: usize = 0;
        let mut current_wait = NTtyReadWait::Forever;
        let mut inter_byte_timeout = None;
        if !ldata.icanon {
            let core = tty.core();
            let termios = core.termios();
            let vmin = termios.control_characters[ControlCharIndex::VMIN] as usize;
            let vtime = termios.control_characters[ControlCharIndex::VTIME] as u64;
            minimum = vmin;
            if vmin == 0 {
                minimum = 1;
                current_wait = if vtime == 0 {
                    NTtyReadWait::NoWait
                } else {
                    NTtyReadWait::Timeout(Duration::from_millis(vtime * 100))
                };
            } else if vtime != 0 {
                inter_byte_timeout = Some(Duration::from_millis(vtime * 100));
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
                drop(ldata);
                let _ = pty_drain_pending_to(tty.clone());
                ldata = self.disc_data();
            }

            if !ldata.input_available(core.termios(), false) {
                if Self::packet_status_pending(core, packet) {
                    drop(ldata);
                    continue;
                }

                {
                    let flags = core.flags();
                    if flags.contains(TtyFlag::OTHER_CLOSED) {
                        if flags.contains(TtyFlag::HUPPED) || flags.contains(TtyFlag::HUPPING) {
                            break;
                        }
                        ret = Err(SystemError::EIO);
                        break;
                    }

                    if flags.contains(TtyFlag::HUPPED) || flags.contains(TtyFlag::HUPPING) {
                        break;
                    }
                }

                if matches!(current_wait, NTtyReadWait::NoWait) {
                    break;
                }

                if flags.contains(FileFlags::O_NONBLOCK)
                    || core.flags().contains(TtyFlag::LDISC_CHANGING)
                {
                    ret = Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    break;
                }

                if ProcessManager::current_pcb().has_pending_signal_fast() {
                    ret = Err(SystemError::ERESTARTSYS);
                    break;
                }

                drop(ldata);
                let events = (EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as u64;
                let readiness = || {
                    let ldata = self.disc_data();
                    ldata.input_available(core.termios(), false)
                        || Self::packet_status_pending(core, packet)
                        || core.flags().contains(TtyFlag::OTHER_CLOSED)
                        || core.flags().contains(TtyFlag::HUPPED)
                        || core.flags().contains(TtyFlag::HUPPING)
                        || core.flags().contains(TtyFlag::LDISC_CHANGING)
                        || ProcessManager::current_pcb().has_pending_signal_fast()
                };
                let wait_result = match current_wait {
                    NTtyReadWait::NoWait => Ok(()),
                    NTtyReadWait::Forever => {
                        core.read_wq().wait_event_interruptible(events, readiness)
                    }
                    NTtyReadWait::Timeout(timeout) => core
                        .read_wq()
                        .wait_event_interruptible_timeout(events, readiness, timeout),
                };
                if let Err(err) = wait_result {
                    if err != SystemError::EAGAIN_OR_EWOULDBLOCK {
                        ret = Err(err);
                    }
                    break;
                }
                continue;
            }

            if ldata.icanon && !core.termios().local_mode.contains(LocalMode::EXTPROC) {
                let more = ldata.canon_copy_from_read_buf(buf, &mut nr, &mut offset)?;
                if more {
                    *cookie = true;
                    break;
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
                    break;
                }
            }

            if offset >= minimum {
                break;
            }
            if let Some(timeout) = inter_byte_timeout {
                if offset != 0 {
                    current_wait = NTtyReadWait::Timeout(timeout);
                }
            }
        }
        let ldata = self.disc_data();
        let read_tail_moved = tail != ldata.read_tail;
        drop(ldata);
        if read_tail_moved {
            tty_kick_input_worker(tty.clone());
            Self::check_pty_unthrottle_after_read(&tty);
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
        _flags: FileFlags,
    ) -> Result<usize, system_error::SystemError> {
        let mut nr = len;
        let mut out_buf = Vec::with_capacity(NTTY_BUFSIZE);
        let pcb = ProcessManager::current_pcb();
        let binding = tty.clone();
        let core = binding.core();
        let mut termios = *core.termios();
        if termios.local_mode.contains(LocalMode::TOSTOP) {
            TtyJobCtrlManager::tty_check_change(tty.clone(), Signal::SIGTTOU)?;
        }

        let mut output_guard = Some(self.output_lock.lock());

        self.disc_data().process_echoes(tty.clone());
        self.drain_echoes(&tty)?;

        let mut offset = 0;
        loop {
            if pcb.has_pending_signal_fast() {
                if offset != 0 {
                    break;
                }
                return Err(SystemError::ERESTARTSYS);
            }
            if core.flags().contains(TtyFlag::HUPPED)
                || (core.flags().contains(TtyFlag::OTHER_CLOSED)
                    && core.driver().tty_driver_sub_type() != TtyDriverSubType::PtyMaster)
                || core.flags().contains(TtyFlag::HUPPING)
            {
                if offset != 0 {
                    break;
                }
                return Err(SystemError::EIO);
            }
            if termios.output_mode.contains(OutputMode::OPOST) {
                let mut made_progress = false;
                out_buf.clear();
                let pending = self.disc_data().opost_pending_bytes().to_vec();
                out_buf.extend_from_slice(&pending);

                if !out_buf.is_empty() {
                    let written = tty.write(core, &out_buf, out_buf.len())?;
                    if written != 0 {
                        let mut guard = self.disc_data();
                        guard.advance_opost_pending(written);
                        made_progress = true;
                    }
                    if written != 0 {
                        tty.flush_chars(core);
                    }
                } else {
                    out_buf.clear();
                    let space = tty.write_room(core).min(out_buf.capacity());
                    let simple_len = if space == 0 {
                        0
                    } else {
                        let guard = self.disc_data();
                        guard.simple_output_block_len(&termios, &buf[offset..], space.min(nr))
                    };

                    if simple_len != 0 {
                        let written =
                            tty.write(core, &buf[offset..offset + simple_len], simple_len)?;
                        if written != 0 {
                            self.disc_data().apply_simple_output_columns(
                                &termios,
                                &buf[offset..offset + written],
                            );
                            offset += written;
                            nr -= written;
                            made_progress = true;
                            tty.flush_chars(core);
                        }
                    } else if space != 0 && nr != 0 {
                        let mut guard = self.disc_data();
                        let cursor_column = guard.cursor_column;
                        let canon_cursor_column = guard.canon_cursor_column;
                        let opost_result = guard.process_output_char_to_buf(
                            &termios,
                            buf[offset],
                            &mut out_buf,
                            space,
                        );
                        if opost_result == OpostCharResult::NeedsMoreRoom {
                            guard.cursor_column = cursor_column;
                            guard.canon_cursor_column = canon_cursor_column;
                        }
                        drop(guard);

                        if opost_result == OpostCharResult::ConsumedWithoutOutput {
                            offset += 1;
                            nr -= 1;
                            made_progress = true;
                        } else if opost_result == OpostCharResult::Emitted {
                            let mut sent = 0;
                            while sent < out_buf.len() {
                                let written =
                                    tty.write(core, &out_buf[sent..], out_buf.len() - sent)?;
                                if written == 0 {
                                    if sent == 0 {
                                        let mut guard = self.disc_data();
                                        guard.cursor_column = cursor_column;
                                        guard.canon_cursor_column = canon_cursor_column;
                                        break;
                                    } else {
                                        let mut guard = self.disc_data();
                                        guard.opost_pending.clear();
                                        guard.opost_pending.extend_from_slice(&out_buf[sent..]);
                                        guard.opost_pending_offset = 0;
                                        break;
                                    }
                                }
                                sent += written;
                                tty.flush_chars(core);
                            }

                            if sent != 0 {
                                offset += 1;
                                nr -= 1;
                                made_progress = true;
                            }
                        }
                    }
                }

                if made_progress {
                    continue;
                }
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

            let opost_pending = termios.output_mode.contains(OutputMode::OPOST)
                && !self.disc_data().opost_pending_bytes().is_empty();
            if nr == 0 && !opost_pending {
                break;
            }

            if _flags.contains(FileFlags::O_NONBLOCK)
                || core.flags().contains(TtyFlag::LDISC_CHANGING)
            {
                if offset != 0 {
                    if self.disc_data().has_output_wakeup_pending() {
                        core.flags_write().insert(TtyFlag::DO_WRITE_WAKEUP);
                    }
                    break;
                }
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            // 到这里表明没位置可写了
            // 休眠一段时间
            // 获取到termios读锁，避免termios被更改导致行为异常
            drop(output_guard.take());
            let wait_result = core.write_wq().wait_event_interruptible(
                EPollEventType::EPOLLOUT.bits() as u64,
                || {
                    if core.flags().contains(TtyFlag::HUPPED)
                        || (core.flags().contains(TtyFlag::OTHER_CLOSED)
                            && core.driver().tty_driver_sub_type() != TtyDriverSubType::PtyMaster)
                        || core.flags().contains(TtyFlag::HUPPING)
                        || core.flags().contains(TtyFlag::LDISC_CHANGING)
                    {
                        return true;
                    }

                    let write_room = tty.write_room(core);
                    if !termios.output_mode.contains(OutputMode::OPOST) {
                        return write_room > 0;
                    }

                    let guard = self.disc_data();
                    let next_input = if nr != 0 { Some(buf[offset]) } else { None };
                    guard.opost_progress_possible(&termios, next_input, write_room)
                },
            );
            if let Err(err) = wait_result {
                output_guard = Some(self.output_lock.lock());
                if offset != 0 {
                    break;
                }
                return Err(err);
            }
            output_guard = Some(self.output_lock.lock());
            termios = *core.termios();
        }

        if self.disc_data().has_output_wakeup_pending() {
            core.flags_write().insert(TtyFlag::DO_WRITE_WAKEUP);
        }

        drop(output_guard);
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

                let count = tty.chars_in_buffer(tty.core());
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

            // 非规范模式或没有积压输入时不需要伪 EOF；否则提交当前输入。
            if !termios.local_mode.contains(LocalMode::ICANON) || ldata.read_cnt() == 0 {
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
        ldata.echo = termios.local_mode.contains(LocalMode::ECHO);

        if termios.input_mode.contains(InputMode::ISTRIP)
            || termios.input_mode.contains(InputMode::IUCLC)
            || termios.input_mode.contains(InputMode::IGNCR)
            || termios.input_mode.contains(InputMode::ICRNL)
            || termios.input_mode.contains(InputMode::INLCR)
            || termios.local_mode.contains(LocalMode::ICANON)
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
                .set(ControlCharIndex::DISABLE_CHAR as usize, false);
            ldata.raw = false;
            ldata.real_raw = false;
        } else {
            // 原模式或real_raw
            ldata.raw = true;

            ldata.real_raw = (termios.input_mode.contains(InputMode::IGNBRK)
                || (!termios.input_mode.contains(InputMode::BRKINT)
                    && !termios.input_mode.contains(InputMode::PARMRK)))
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
        let mut ldata = self.disc_data();

        let mut event = EPollEventType::empty();
        if ldata.input_available(core.termios(), true) {
            event.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM)
        } else {
            drop(ldata);
            let _ = pty_drain_pending_to(tty.clone());
            ldata = self.disc_data();
            if ldata.input_available(core.termios(), true) {
                event.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM)
            }
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

        if !core.write_lock().is_locked()
            && core.driver().driver_funcs().chars_in_buffer(core) < 256
            && core.driver().driver_funcs().write_room(core) > 0
        {
            event.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
        }

        Ok(event.bits() as usize)
    }

    fn write_wakeup(&self, tty: &TtyCore) -> Result<(), SystemError> {
        if let Some(_output_guard) = self.output_lock.try_lock() {
            if self.drain_opost_pending(tty)? {
                self.drain_echoes(tty)?;
            }
        }
        Ok(())
    }

    fn receive_room(&self, tty: Arc<TtyCore>) -> usize {
        let termios = tty.core().termios();
        let ldata = self.disc_data();
        let tail = ldata.read_tail;
        let mut room = NTTY_BUFSIZE as isize - (ldata.read_head - tail) as isize;

        if termios.input_mode.contains(InputMode::PARMRK) {
            room = if room > 0 { (room + 2) / 3 } else { room };
        }

        room -= 1;
        if room <= 0 {
            let overflow = ldata.icanon && ldata.canon_head == tail;
            room = if overflow { 1 } else { 0 };
        }

        room as usize
    }

    fn hangup(&self, tty: Arc<TtyCore>) -> Result<(), system_error::SystemError> {
        self.flush_buffer(tty.clone())?;
        self.flush_output(tty.clone())?;
        tty.core().read_wq().wakeup_all();
        tty.core().write_wq().wakeup_all();
        if let Some(link) = tty.core().link() {
            link.core().read_wq().wakeup_all();
            link.core().write_wq().wakeup_all();
        }
        Ok(())
    }

    fn receive_buf(
        &self,
        tty: Arc<TtyCore>,
        buf: &[u8],
        flags: Option<&[u8]>,
        count: usize,
    ) -> Result<usize, SystemError> {
        let mut ldata = self.disc_data();
        let ret = ldata.receive_buf_common(tty.clone(), buf, flags, count, false);
        let deferred_tty_wakeup = ldata.take_deferred_tty_wakeup();
        drop(ldata);
        if deferred_tty_wakeup {
            tty.tty_wakeup();
        }
        if let Some(_output_guard) = self.output_lock.try_lock() {
            self.drain_echoes(&tty)?;
        }
        ret
    }

    fn receive_buf2(
        &self,
        tty: Arc<TtyCore>,
        buf: &[u8],
        flags: Option<&[u8]>,
        count: usize,
    ) -> Result<usize, SystemError> {
        let mut ldata = self.disc_data();
        let ret = ldata.receive_buf_common(tty.clone(), buf, flags, count, true);
        let deferred_tty_wakeup = ldata.take_deferred_tty_wakeup();
        drop(ldata);
        if deferred_tty_wakeup {
            tty.tty_wakeup();
        }
        if let Some(_output_guard) = self.output_lock.try_lock() {
            self.drain_echoes(&tty)?;
        }
        ret
    }
}
