use alloc::{sync::Arc, vec::Vec};

use crate::{
    libs::wait_queue::WaitQueue,
    process::{ProcessControlBlock, ProcessManager},
    syscall::{
        user_access::{UserBufferReader, UserBufferWriter},
        Syscall, SystemError,
    },
    time::{timekeep::ktime_t, TimeSpec, MSEC_PER_SEC, NSEC_PER_MSEC},
};

use super::{file::File, PollStatus};

/// @brief PollWqueues 对外的接口
pub trait PollTable {
    /// @brief 文件的 poll() 函数内部会调用该函数使得当前进程挂载到对应的等待队列
    fn poll_wait(&mut self, file: Arc<File>, wait_queue: Arc<WaitQueue>);
}

/// @brief 每个调用 select/poll 的进程都会维护一个 PollWqueues，用于轮询
struct PollWqueues {
    /// 是否执行挂载等待队列并阻塞
    pt: bool,
    /// 关注的事件
    key: PollStatus,
    /// 进行轮询的进程
    polling_task: Arc<ProcessControlBlock>,
    triggered: bool,
    error: Option<SystemError>,
    poll_table: Vec<PollTableEntry>,
}

impl PollWqueues {
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

    /// @brief 在 poll 等待期间设置超时
    fn poll_schedule_timeout(&self, expires: ktime_t) -> Result<(), SystemError> {
        todo!()
    }
}

impl PollTable for PollWqueues {
    fn poll_wait(&mut self, file: Arc<File>, wait_queue: Arc<WaitQueue>) {
        if self.pt == false {
            return;
        }

        let entry = PollTableEntry::new(file, self.key, wait_queue);
        self.poll_table.push(entry);
        // TODO: 将当前进程加入等待队列
    }
}

impl Drop for PollWqueues {
    /// @brief 将 poll_table 的所有元素从对应的等待队列中移除
    fn drop(&mut self) {
        todo!()
    }
}

/// @brief 对应每一个 IO 监听事件
struct PollTableEntry {
    file: Arc<File>,
    key: PollStatus,
    wait_queue: Arc<WaitQueue>,
    // TODO: 等待队列项
}

impl PollTableEntry {
    fn new(file: Arc<File>, key: PollStatus, wait_queue: Arc<WaitQueue>) -> Self {
        Self {
            file,
            key,
            wait_queue,
        }
    }
}

impl Drop for PollTableEntry {
    /// @brief 将自身从对应的等待队列中移除
    fn drop(&mut self) {
        todo!()
    }
}

/// @brief 唤醒函数
fn poll_wake() {
    todo!()
}

/// @brief 系统调用 poll 使用的事件监视结构体
#[derive(Clone, Copy)]
#[repr(C)]
pub struct PosixPollfd {
    fd: i32,        // 监视的文件描述符
    events: u16,    // 关注的事件类型
    revents: u16,   // 返回的事件类型
}

impl From<Pollfd> for PosixPollfd {
    fn from(value: Pollfd) -> Self {
        Self {
            fd: value.fd,
            events: value.events.bits(),
            revents: value.revents.bits(),
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
            events: PollStatus::from_bits_truncate(value.events),
            revents: PollStatus::from_bits_truncate(value.revents),
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
        drop(poll_wqueues);

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
    fn do_poll(
        nfds: usize,
        poll_list: &mut PollList,
        poll_wqueues: &mut PollWqueues,
        end_time: Option<TimeSpec>,
    ) -> Result<usize, SystemError> {
        let mut fdcount: usize = 0; // 监听事件发生的数量
        let mut timed_out = false; // 是否已经超时
        let mut busy_flag = PollStatus::empty(); // TODO: 设置允许忙等待标志 busy_flag

        // 优化无需阻塞的情景
        if end_time.is_none() || (end_time.unwrap().tv_sec == 0 && end_time.unwrap().tv_nsec == 0) {
            poll_wqueues.pt = false;
            timed_out = true;
        }

        // 轮询
        loop {
            let mut can_busy_loop = false;
            for pollfd in poll_list.entries.iter_mut() {
                let mask = Self::do_pollfd(pollfd, poll_wqueues, &mut can_busy_loop, busy_flag)?;
                if !mask.is_empty() {
                    fdcount += 1;
                    // 已找到目标事件，无需再进行阻塞或忙等待
                    poll_wqueues.pt = false;
                    busy_flag = PollStatus::empty(); 
                    can_busy_loop = false;
                }
            }
            
            // 所有的监听已经注册挂载，无需再阻塞进行挂载
            poll_wqueues.pt = false;
            if fdcount == 0 {
                // TODO: 有待处理信号
                unimplemented!();
            }
            // 有事件响应或已经超时
            if fdcount > 0 || timed_out == true {
                break;
            }
            // TODO: 忙等待机制

            // 在第一次循环结束时，即进入阻塞前，设置超时时间
            if let Some(end_time) = end_time {
                todo!();
            };

            // 阻塞进行调度
            // poll_wqueues.poll_schedule_timeout(expires)?;
            timed_out = true;
        }

        return Ok(fdcount);
    }

    /// @brief 对 pollfd 调用对应的 poll() 方法
    fn do_pollfd(
        pollfd: &mut Pollfd,
        poll_wqueues: &mut PollWqueues,
        _can_busy_loop: &mut bool,
        _busy_flag: PollStatus,
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
        let mask = file_guard.inode().poll()?;
        pollfd.revents = mask;

        // TODO: 忙等待机制

        return Ok(mask);
    }
}
