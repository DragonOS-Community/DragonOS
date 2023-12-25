use alloc::{sync::Arc, vec::Vec};

use crate::{
    arch::sched::{self, sched},
    libs::spinlock::SpinLock,
    process::{ProcessControlBlock, ProcessManager},
    sched::core::do_sched,
    syscall::{
        user_access::{UserBufferReader, UserBufferWriter},
        Syscall, SystemError,
    },
    time::{TimeSpec, MSEC_PER_SEC, NSEC_PER_MSEC},
};

use super::{file::File, PollStatus};

/// @brief PollWqueues 对外的接口
pub trait PollTable {
    /// @brief 文件的 poll() 函数内部会调用该函数使得当前进程挂载到对应的等待队列
    fn poll_wait(&self, file: Arc<File>, wait_queue: Arc<SpinLock<PollWaitQueue>>);
    fn poll_freewait(&self);
    fn poll_schedule_timeout(&self);
}

/// @brief 每个调用 select/poll 的进程都会维护一个 PollWqueues，用于轮询
struct PollWqueues(Arc<SpinLock<PollWqueuesInner>>);

impl PollWqueues {
    fn new() -> Self {
        Self(Arc::new(SpinLock::new(PollWqueuesInner::new())))
    }
}

impl PollTable for PollWqueues {
    fn poll_wait(&self, file: Arc<File>, wait_queue: Arc<SpinLock<PollWaitQueue>>) {
        if self.0.lock().pt == false {
            return;
        }

        let entry = Arc::new(PollTableEntry::new(
            file,
            self.0.lock().key,
            wait_queue.clone(),
            self.0.clone(),
        ));
        self.0.lock().poll_table.push(entry.clone());
        wait_queue.lock().wait(entry.clone());
    }

    /// @brief 将所有的 entry 从等待队列中删除
    fn poll_freewait(&self) {
        self.0.lock().poll_freewait();
    }

    /// @brief 阻塞当前进程直到被唤醒或超时
    fn poll_schedule_timeout(&self) {
        sched();
    }
}

struct PollWqueuesInner {
    /// 是否执行挂载等待队列并阻塞
    pt: bool,
    /// 关注的事件
    key: PollStatus,
    /// 进行轮询的进程
    polling_task: Arc<ProcessControlBlock>,
    triggered: bool,
    error: Option<SystemError>,
    poll_table: Vec<Arc<PollTableEntry>>,
}

impl PollWqueuesInner {
    /// @brief 初始化 PollWqueues 结构体，对应 Linux 的 poll_init_wait()
    fn new() -> Self {
        Self {
            pt: true,
            key: PollStatus::all(),
            polling_task: ProcessManager::current_pcb(),
            triggered: false,
            error: None,
            poll_table: vec![],
        }
    }

    /// @brief 将所有的 entry 从监听的等待队列中删除
    fn poll_freewait(&mut self) {
        for entry in self.poll_table.iter_mut() {
            entry.wait_queue.lock().remove(entry.clone());
        }
    }
}

/// @brief 对应每一个 IO 监听事件
pub struct PollTableEntry {
    /// 监听的文件
    file: Arc<File>,
    /// 监听的事件
    key: PollStatus,
    /// 所在的等待队列
    wait_queue: Arc<SpinLock<PollWaitQueue>>,
    /// 所在的 PollWqueues
    pwq: Arc<SpinLock<PollWqueuesInner>>,
}

impl PollTableEntry {
    fn new(
        file: Arc<File>,
        key: PollStatus,
        wait_queue: Arc<SpinLock<PollWaitQueue>>,
        pwq: Arc<SpinLock<PollWqueuesInner>>,
    ) -> Self {
        Self {
            file,
            key,
            wait_queue,
            pwq,
        }
    }

    /// @brief 在等待队列唤醒后的回调函数
    fn callback(&self) {}
}

pub struct PollWaitQueue(Vec<Arc<PollTableEntry>>);

impl PollWaitQueue {
    pub fn new() -> Self {
        Self(vec![])
    }

    /// @brief 标记当前进程为睡眠状态但不进行调度
    pub fn wait(&mut self, entry: Arc<PollTableEntry>) {
        self.0.push(entry);
        ProcessManager::mark_sleep(false).unwrap_or_else(|e| {
            panic!("sleep error: {:?}", e);
        });
    }

    /// @brief 唤醒等待队列中满足指定监听事件的 entry
    pub fn wakeup(&mut self, key: PollStatus) {
        let mut temp = vec![];

        for entry in self.0.iter_mut() {
            if entry.key & key != PollStatus::empty() {
                temp.push(entry.clone());
                // 唤醒对应的进程
                ProcessManager::wakeup(&entry.pwq.lock().polling_task);
            }
        }

        // 从等待队列中删除
        self.0.extract_if(|x| x.key & key != PollStatus::empty());
        // 进行回调函数
        for entry in temp.iter_mut() {
            entry.callback();
        }
    }

    /// @brief 在等待队列中删除指定元素
    fn remove(&mut self, entry: Arc<PollTableEntry>) {
        self.0.extract_if(|x| Arc::as_ptr(x) == Arc::as_ptr(&entry));
    }
}

/// @brief 系统调用 poll 使用的事件监视结构体
#[derive(Clone, Copy)]
#[repr(C)]
pub struct PosixPollfd {
    fd: i32,      // 监视的文件描述符
    events: u16,  // 关注的事件类型
    revents: u16, // 返回的事件类型
}

impl From<Pollfd> for PosixPollfd {
    fn from(value: Pollfd) -> Self {
        Self {
            fd: value.fd,
            events: value.events.bits() as u16,
            revents: value.revents.bits() as u16,
        }
    }
}

struct Pollfd {
    fd: i32,
    events: PollStatus,
    revents: PollStatus,
}

impl From<PosixPollfd> for Pollfd {
    fn from(value: PosixPollfd) -> Self {
        Self {
            fd: value.fd,
            events: PollStatus::from_bits_truncate(value.events as u8),
            revents: PollStatus::from_bits_truncate(value.revents as u8),
        }
    }
}

struct PollList {
    entries: Vec<Pollfd>,
}

impl PollList {
    fn new() -> Self {
        Self { entries: vec![] }
    }
}

/// 计算超时时间
fn poll_select_set_timeout(timeout_msecs: i32) -> Option<TimeSpec> {
    let sec: i64 = timeout_msecs as i64 / MSEC_PER_SEC as i64;
    let nsec: i64 = NSEC_PER_MSEC as i64 * (timeout_msecs as i64 % MSEC_PER_SEC as i64);
    return Some(TimeSpec::new(sec, nsec));
}

impl Syscall {
    /// @brief 系统调用 poll
    /// @param ufds 用户空间的 pollfd 数组
    /// @param nfds pollfd 数组的长度
    /// @param timeout_msecs 超时时间，以微妙为单位
    /// @return 发生事件的个数 / 错误码
    pub fn poll(
        ufds: *mut PosixPollfd,
        nfds: usize,
        timeout_msecs: i32,
    ) -> Result<usize, SystemError> {
        let end_time = if timeout_msecs >= 0 {
            poll_select_set_timeout(timeout_msecs)
        } else {
            None
        };

        let ret = Self::do_sys_poll(ufds, nfds, end_time);

        if let Err(err) = ret {
            match err {
                SystemError::EINTR => {
                    // TODO: 如果被信号打断，则重启系统调用
                    unimplemented!()
                }
                _ => return Err(err),
            }
        }

        return ret;
    }

    /// @brief 将用户空间的 pollfd 数组拷贝到内核空间，执行 do_poll() 后，再将数组拷贝会用户空间
    fn do_sys_poll(
        ufds: *mut PosixPollfd,
        nfds: usize,
        end_time: Option<TimeSpec>,
    ) -> Result<usize, SystemError> {
        let mut poll_wqueues = PollWqueues::new();
        let mut poll_list = PollList::new();

        // 将 pollfd 从用户空间拷贝到内核空间
        let user_reader = UserBufferReader::new(ufds, nfds, true)?;
        let buffer: &[PosixPollfd] = user_reader.read_from_user(0)?;
        for &pollfd in buffer {
            poll_list.entries.push(Pollfd::from(pollfd));
        }
        drop(user_reader);

        let fdcount = Self::do_poll(nfds, &mut poll_list, &mut poll_wqueues, end_time)?;
        poll_wqueues.poll_freewait();

        // 将 pollfd 从内核空间拷贝会用户空间
        let mut user_writer = UserBufferWriter::new(ufds, nfds, true)?;
        let mut buffer: Vec<PosixPollfd> = vec![];
        for pollfd in poll_list.entries {
            buffer.push(PosixPollfd::from(pollfd));
        }
        user_writer.copy_to_user(&buffer, 0)?;

        return Ok(fdcount);
    }

    /// @brief 轮询监听的文件描述符
    /// TODO: 允许忙等待机制
    /// TODO: 处理信号
    fn do_poll(
        nfds: usize,
        poll_list: &mut PollList,
        poll_wqueues: &mut PollWqueues,
        end_time: Option<TimeSpec>,
    ) -> Result<usize, SystemError> {
        let mut fdcount: usize = 0; // 监听事件发生的数量
        let mut timed_out = false; // 是否已经超时

        // 优化无需阻塞的情景
        if end_time.is_none() || (end_time.unwrap().tv_sec == 0 && end_time.unwrap().tv_nsec == 0) {
            poll_wqueues.0.lock().pt = false;
            timed_out = true;
        }

        // 轮询
        loop {
            for pollfd in poll_list.entries.iter_mut() {
                let mask = Self::do_pollfd(pollfd, poll_wqueues)?;
                if !mask.is_empty() {
                    fdcount += 1;
                    // 已找到目标事件，无需再进行阻塞或忙等待
                    poll_wqueues.0.lock().pt = false;
                }
            }

            // 所有的监听已经注册挂载，无需再阻塞进行挂载
            poll_wqueues.0.lock().pt = false;
            // 有事件响应或已经超时
            if fdcount > 0 || timed_out == true {
                break;
            }

            // 在第一次循环结束时，即进入阻塞前，设置超时时间
            if let Some(end_time) = end_time {
                todo!();
            };

            // 阻塞进行调度，被唤醒时要么是超时，要么是监听到事件发生
            poll_wqueues.poll_schedule_timeout();
            timed_out = true;
        }

        return Ok(fdcount);
    }

    /// @brief 对 pollfd 调用对应的 poll() 方法
    /// TODO: 允许忙等待机制
    fn do_pollfd(
        pollfd: &mut Pollfd,
        _poll_wqueues: &mut PollWqueues,
    ) -> Result<PollStatus, SystemError> {
        let fd = pollfd.fd;

        if fd < 0 {
            return Err(SystemError::EINVAL);
        }

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;
        let file_guard = file.lock();

        // TODO: 等待队列机制
        let mask = file_guard.inode().poll()? as PollStatus;
        pollfd.revents = mask;

        return Ok(mask);
    }
}
