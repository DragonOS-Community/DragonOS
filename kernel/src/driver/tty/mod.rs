use core::intrinsics::unlikely;

use alloc::string::String;

use kdepends::thingbuf::mpsc::{
    self,
    errors::{TryRecvError, TrySendError},
};

use crate::libs::rwlock::RwLock;

pub mod init;
pub mod serial;
pub mod tty_device;
pub mod tty_driver;
pub mod vt;

bitflags! {
    pub struct TtyCoreState: u32{
        /// 在读取stdin缓冲区时，由于队列为空，有读者被阻塞
        const BLOCK_AT_STDIN_READ = (1 << 0);
        /// 开启输入回显。
        const ECHO_ON = (1 << 1);
    }

    #[derive(Default)]
    pub struct TtyFileFlag:u32{
        /// 当前文件是stdin文件
        const STDIN = (1 << 0);
        /// 当前文件是stdout文件
        const STDOUT = (1 << 1);
        /// 当前文件是stderr文件
        const STDERR = (1 << 2);
    }
}

/// @brief tty文件的私有信息
#[derive(Debug, Default, Clone)]
pub struct TtyFilePrivateData {
    flags: TtyFileFlag,
}

/// @brief tty设备的核心功能结构体。在此结构体的基础上，衍生出TTY/PTY/PTS等
///
/// 每个TTY Core有5个端口：
/// - stdin：连接到一个活动进程的stdin文件描述符
/// - stdout：连接到多个进程的stdout文件描述符
/// - stderr：连接到多个进程的stdout文件描述符
/// - 输入端口：向tty设备输入数据的接口。输入到该接口的数据，将被导向stdin接口。
///     如果开启了回显，那么，数据也将同时被导向输出端
/// - 输出端口：tty设备对外输出数据的端口。从stdout、stderr输入的数据，将会被导向此端口。
///             此端口可以连接到屏幕、文件、或者是另一个tty core的输入端口。如果开启了
///             输入数据回显，那么，输入端口的数据，将会被同时导向此端口，以及stdin端口
#[derive(Debug)]
struct TtyCore {
    /// stdin的mpsc队列输入输出端
    stdin_rx: mpsc::Receiver<u8>,
    stdin_tx: mpsc::Sender<u8>,
    /// 输出的mpsc队列输入输出端
    output_rx: mpsc::Receiver<u8>,
    output_tx: mpsc::Sender<u8>,
    // 前台进程,以后改成前台进程组
    // front_job: Option<Pid>,
    /// tty核心的状态
    state: RwLock<TtyCoreState>,
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum TtyError {
    /// 缓冲区满,返回成功传送的字节数
    BufferFull(usize),
    /// 缓冲区空，返回成功传送的字节数
    BufferEmpty(usize),
    /// 设备已经被关闭
    Closed,
    /// End of file(已经读取的字符数，包含eof)
    EOF(usize),
    /// 接收到信号终止
    Stopped(usize),
    Unknown(String),
}

impl TtyCore {
    // 各个缓冲区的大小
    pub const STDIN_BUF_SIZE: usize = 4096;
    pub const OUTPUT_BUF_SIZE: usize = 4096;

    /// @brief 创建一个TTY核心组件
    pub fn new() -> TtyCore {
        let (stdin_tx, stdin_rx) = mpsc::channel::<u8>(Self::STDIN_BUF_SIZE);
        let (output_tx, output_rx) = mpsc::channel::<u8>(Self::OUTPUT_BUF_SIZE);
        let state: RwLock<TtyCoreState> = RwLock::new(TtyCoreState { bits: 0 });

        return TtyCore {
            stdin_rx,
            stdin_tx,
            output_rx,
            output_tx,
            state,
        };
    }

    /// @brief 向tty的输入端口输入数据
    ///
    /// @param buf 输入数据
    ///
    /// @param block 是否允许阻塞
    ///
    /// @return Ok(成功传送的字节数)
    /// @return Err(TtyError) 内部错误信息
    pub fn input(&self, buf: &[u8], block: bool) -> Result<usize, TtyError> {
        let val = self.write_stdin(buf, block)?;
        // 如果开启了输入回显，那么就写一份到输出缓冲区
        if self.echo_enabled() {
            self.write_output(&buf[0..val], true)?;
        }
        return Ok(val);
    }

    /// @brief 从tty的输出端口读出数据
    ///
    /// @param buf 输出缓冲区
    ///
    /// @return Ok(成功传送的字节数)
    /// @return Err(TtyError) 内部错误信息
    #[inline]
    pub fn output(&self, buf: &mut [u8], block: bool) -> Result<usize, TtyError> {
        return self.read_output(buf, block);
    }

    /// @brief tty的stdout接口
    ///
    /// @param buf 输入缓冲区
    ///
    /// @return Ok(成功传送的字节数)
    /// @return Err(TtyError) 内部错误信息
    #[inline]
    pub fn stdout(&self, buf: &[u8], block: bool) -> Result<usize, TtyError> {
        return self.write_output(buf, block);
    }

    /// @brief tty的stderr接口
    ///
    /// @param buf 输入缓冲区
    ///
    /// @return Ok(成功传送的字节数)
    /// @return Err(TtyError) 内部错误信息
    #[inline]
    pub fn stderr(&self, buf: &[u8], block: bool) -> Result<usize, TtyError> {
        return self.write_output(buf, block);
    }

    /// @brief 读取TTY的stdin缓冲区
    ///
    /// @param buf 读取到的位置
    /// @param block 是否阻塞读
    ///
    /// @return Ok(成功读取的字节数)
    /// @return Err(TtyError) 内部错误信息
    pub fn read_stdin(&self, buf: &mut [u8], block: bool) -> Result<usize, TtyError> {
        // TODO: 增加对EOF的处理
        let mut cnt = 0;
        while cnt < buf.len() {
            let val: Result<mpsc::RecvRef<u8>, TryRecvError> = self.stdin_rx.try_recv_ref();
            if let Err(err) = val {
                match err {
                    TryRecvError::Closed => return Err(TtyError::Closed),
                    TryRecvError::Empty => {
                        if block {
                            continue;
                        } else {
                            return Ok(cnt);
                        }
                    }
                    _ => return Err(TtyError::Unknown(format!("{err:?}"))),
                }
            } else {
                let x = *val.unwrap();
                buf[cnt] = x;
                cnt += 1;

                if unlikely(self.stdin_should_return(x)) {
                    return Ok(cnt);
                }
            }
        }
        return Ok(cnt);
    }

    fn stdin_should_return(&self, c: u8) -> bool {
        // 如果是换行符或者是ctrl+d，那么就应该返回
        return c == b'\n' || c == 4;
    }

    /// @brief 向stdin缓冲区内写入数据
    ///
    /// @param buf 输入缓冲区
    ///
    /// @param block 当缓冲区满的时候，是否阻塞
    ///
    /// @return Ok(成功传送的字节数)
    /// @return Err(BufferFull(成功传送的字节数)) 缓冲区满，成功传送的字节数
    /// @return Err(TtyError) 内部错误信息
    fn write_stdin(&self, buf: &[u8], block: bool) -> Result<usize, TtyError> {
        let mut cnt = 0;
        while cnt < buf.len() {
            let r: Result<mpsc::SendRef<u8>, TrySendError> = self.stdin_tx.try_send_ref();
            if let Err(e) = r {
                match e {
                    TrySendError::Closed(_) => return Err(TtyError::Closed),
                    TrySendError::Full(_) => {
                        if block {
                            continue;
                        } else {
                            return Err(TtyError::BufferFull(cnt));
                        }
                    }
                    _ => return Err(TtyError::Unknown(format!("{e:?}"))),
                }
            } else {
                *r.unwrap() = buf[cnt];
                cnt += 1;
            }
        }

        return Ok(cnt);
    }

    /// @brief 读取TTY的output缓冲区
    ///
    /// @param buf 读取到的位置
    /// @param block 是否阻塞读
    ///
    /// @return Ok(成功读取的字节数)
    /// @return Err(TtyError) 内部错误信息
    fn read_output(&self, buf: &mut [u8], block: bool) -> Result<usize, TtyError> {
        let mut cnt = 0;
        while cnt < buf.len() {
            let val: Result<mpsc::RecvRef<u8>, TryRecvError> = self.output_rx.try_recv_ref();
            if let Err(err) = val {
                match err {
                    TryRecvError::Closed => return Err(TtyError::Closed),
                    TryRecvError::Empty => {
                        if block {
                            continue;
                        } else {
                            return Ok(cnt);
                        }
                    }
                    _ => return Err(TtyError::Unknown(format!("{err:?}"))),
                }
            } else {
                buf[cnt] = *val.unwrap();
                cnt += 1;
            }
        }
        return Ok(cnt);
    }

    /// @brief 向output缓冲区内写入数据
    ///
    /// @param buf 输入缓冲区
    ///
    /// @param block 当缓冲区满的时候，是否阻塞
    ///
    /// @return Ok(成功传送的字节数)
    /// @return Err(BufferFull(成功传送的字节数)) 缓冲区满，成功传送的字节数
    /// @return Err(TtyError) 内部错误信息
    fn write_output(&self, buf: &[u8], block: bool) -> Result<usize, TtyError> {
        let mut cnt = 0;
        while cnt < buf.len() {
            let r: Result<mpsc::SendRef<u8>, TrySendError> = self.output_tx.try_send_ref();
            if let Err(e) = r {
                match e {
                    TrySendError::Closed(_) => return Err(TtyError::Closed),
                    TrySendError::Full(_) => {
                        if block {
                            continue;
                        } else {
                            return Err(TtyError::BufferFull(cnt));
                        }
                    }
                    _ => return Err(TtyError::Unknown(format!("{e:?}"))),
                }
            } else {
                // TODO: 在这里考虑增加对信号发送的处理
                // if buf[cnt] == 3 {
                //     let pid = ProcessManager::current_pcb().pid();
                //     Signal::SIGKILL.send_signal_info(
                //         Some(&mut SigInfo::new(
                //             Signal::SIGKILL,
                //             0,
                //             SigCode::SI_USER,
                //             SigType::Kill(pid),
                //         )),
                //         pid,
                //     );
                //     return Err(TtyError::Stopped(cnt));
                // }
                *r.unwrap() = buf[cnt];
                cnt += 1;
            }
        }
        return Ok(cnt);
    }

    /// @brief 开启tty输入回显（也就是将输入数据传送一份到输出缓冲区）
    #[inline]
    pub fn enable_echo(&self) {
        self.state.write().set(TtyCoreState::ECHO_ON, true);
    }

    /// @brief 关闭输入回显
    #[inline]
    #[allow(dead_code)]
    pub fn disable_echo(&self) {
        self.state.write().set(TtyCoreState::ECHO_ON, false);
    }

    /// @brief 判断当前tty核心，是否开启了输入回显
    ///
    /// @return true 开启了输入回显
    ///
    /// @return false 未开启输入回显
    #[inline]
    #[allow(dead_code)]
    pub fn echo_enabled(&self) -> bool {
        return self.state.read().contains(TtyCoreState::ECHO_ON);
    }
}

// ======= 以下代码考虑了“缓冲区满，然后睡眠，当缓冲区有空位就唤醒”的逻辑。
// 但是由于在开发过程中的调整，并且由于数据结构发生变化，因此暂时不实现上述优化，因此先注释。
//
// @brief 读取TTY的stdin缓冲区
//
// @param buf 读取到的位置
// @param block 是否阻塞读
//
// @return Ok(成功读取的字节数)
// @return Err(TtyError) 内部错误信息
// pub fn read_stdin(&mut self, buf: &mut [u8], block: bool) -> Result<usize, TtyError> {
//     let mut cnt = 0;
//     loop{
//         if cnt == buf.len(){
//             break;
//         }
//         let val:Option<u8> = self.stdin_queue.dequeue();
//         // 如果没读到
//         if val.is_none() {
//             // 如果阻塞读
//             if block {
//                 let state_guard: RwLockUpgradableGuard<TtyCoreState> =
//                     self.state.upgradeable_read();
//                 // 判断是否有进程正在stdin上睡眠，如果有，则忙等读
//                 // 理论上，这种情况应该不存在，因为stdin是单读者的
//                 if state_guard.contains(TtyCoreState::BLOCK_AT_STDIN_READ) {
//                     kwarn!("Read stdin: Process {} want to read its' stdin, but previous process {} is sleeping on the stdin.", current_pcb().pid, self.stdin_waiter.read().as_ref().unwrap().pid);
//                     drop(state_guard);
//                     Self::ringbuf_spin_dequeue(&mut buf[cnt], &mut self.stdin_queue);
//                     cnt += 1;
//                 } else {
//                     // 正常情况，阻塞读，将当前进程休眠
//                     let mut state_guard: RwLockWriteGuard<TtyCoreState> = state_guard.upgrade();
//                     let mut stdin_waiter_guard: RwLockWriteGuard<
//                         Option<&mut process_control_block>,
//                     > = self.stdin_waiter.write();

//                     // 由于输入数据到stdin的时候，必须先获得state guard的读锁。而这里我们已经获取了state的写锁。
//                     // 因此可以保证，此时没有新的数据会进入stdin_queue. 因此再次尝试读取stdin_queue
//                     let val:Option<u8> = self.stdin_queue.dequeue();
//                     // 读到数据，不用睡眠
//                     if val.is_some(){
//                         buf[cnt] = val.unwrap();
//                         cnt += 1;
//                         continue;
//                     }
//                     // 没读到数据，准备睡眠

//                     // 设置等待标志位
//                     state_guard.set(TtyCoreState::BLOCK_AT_STDIN_READ, true);

//                     // 将当前进程标记为被其他机制管理
//                     unsafe {
//                         current_pcb().mark_sleep_interruptible();
//                     }

//                     *stdin_waiter_guard = Some(current_pcb());
//                     drop(stdin_waiter_guard);
//                     drop(state_guard);
//                     sched();
//                     continue;
//                 }
//             } else {
//                 // 非阻塞读，没读到就直接返回了
//                 return Ok(cnt);
//             }
//         }else{
//             buf[cnt] = val.unwrap();
//             cnt += 1;
//         }
//     }

//     return Ok(cnt);
// }

// fn write_stdin(&self)

// /// @brief 非休眠的，自旋地读队列，直到有元素被读出来
// fn ringbuf_spin_dequeue(dst: &mut u8, queue: &mut AllocRingBuffer<u8>) {
//     loop {
//         if let Some(val) = queue.dequeue() {
//             *dst = val;
//             return;
//         }
//     }
// }
