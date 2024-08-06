use hashbrown::HashMap;
use system_error::SystemError;

use crate::{arch::rand::rand, libs::spinlock::SpinLock, process::{Pid, ProcessManager}};

use super::SocketType::{self, *};

/// # TCP 和 UDP 的端口管理器。
/// 如果 TCP/UDP 的 socket 绑定了某个端口，它会在对应的表中记录，以检测端口冲突。
#[derive(Debug)]
pub struct PortManager {
    // TCP 端口记录表
    tcp_port_table: SpinLock<HashMap<u16, Pid>>,
    // UDP 端口记录表
    udp_port_table: SpinLock<HashMap<u16, Pid>>,
}

impl PortManager {
    pub fn new() -> Self {
        return Self {
            tcp_port_table: SpinLock::new(HashMap::new()),
            udp_port_table: SpinLock::new(HashMap::new()),
        };
    }

    /// @brief 自动分配一个相对应协议中未被使用的PORT，如果动态端口均已被占用，返回错误码 EADDRINUSE
    pub fn get_ephemeral_port(&self, socket_type: SocketType) -> Result<u16, SystemError> {
        // TODO: selects non-conflict high port

        static mut EPHEMERAL_PORT: u16 = 0;
        unsafe {
            if EPHEMERAL_PORT == 0 {
                EPHEMERAL_PORT = (49152 + rand() % (65536 - 49152)) as u16;
            }
        }

        let mut remaining = 65536 - 49152; // 剩余尝试分配端口次数
        let mut port: u16;
        while remaining > 0 {
            unsafe {
                if EPHEMERAL_PORT == 65535 {
                    EPHEMERAL_PORT = 49152;
                } else {
                    EPHEMERAL_PORT += 1;
                }
                port = EPHEMERAL_PORT;
            }

            // 使用 ListenTable 检查端口是否被占用
            let listen_table_guard = match socket_type {
                Udp => self.udp_port_table.lock(),
                Tcp => self.tcp_port_table.lock(),
                _ => panic!("{:?} cann't get a port", socket_type),
            };
            if listen_table_guard.get(&port).is_none() {
                drop(listen_table_guard);
                return Ok(port);
            }
            remaining -= 1;
        }
        return Err(SystemError::EADDRINUSE);
    }

    #[inline]
    pub fn bind_ephemeral_port(&self, socket_type: SocketType) -> Result<u16, SystemError> {
        let port = self.get_ephemeral_port(socket_type)?;
        self.bind_port(socket_type, port)?;
        return Ok(port);
    }

    /// @brief 检测给定端口是否已被占用，如果未被占用则在 TCP/UDP 对应的表中记录
    ///
    /// TODO: 增加支持端口复用的逻辑
    pub fn bind_port(&self, socket_type: SocketType, port: u16) -> Result<(), SystemError> {
        if port > 0 {
            match socket_type {
                Udp => {
                    let mut guard = self.udp_port_table.lock();
                    if guard.get(&port).is_some() {
                        return Err(SystemError::EADDRINUSE);
                    }
                    guard.insert(port, ProcessManager::current_pid());
                },
                Tcp => {
                    let mut guard = self.tcp_port_table.lock();
                    if guard.get(&port).is_some() {
                        return Err(SystemError::EADDRINUSE);
                    }
                    guard.insert(port, ProcessManager::current_pid());
                },
                _ => {},
            };
        }
        return Ok(());
    }

    /// @brief 在对应的端口记录表中将端口和 socket 解绑
    /// should call this function when socket is closed or aborted
    pub fn unbind_port(&self, socket_type: SocketType, port: u16) {
        match socket_type {
            Udp => {self.udp_port_table.lock().remove(&port);},
            Tcp => {self.tcp_port_table.lock().remove(&port);},
            _ => {}
        };
    }
}
