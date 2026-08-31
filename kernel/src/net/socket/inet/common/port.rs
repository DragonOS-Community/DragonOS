use core::sync::atomic::{AtomicU16, Ordering};
use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    arch::rand::rand,
    libs::mutex::Mutex,
    process::{ProcessManager, RawPid},
};

use super::Types::{self, *};

/// Per-interface TCP port manager.
///
/// UDP reservations are network-namespace-wide and live in `UdpBindingTable`,
/// because Linux device-bound sockets can legally share a port across ifaces.
#[derive(Debug)]
pub struct PortManager {
    // TCP 端口记录表
    tcp_port_table: Mutex<HashMap<u16, RawPid>>,
}

impl Default for PortManager {
    fn default() -> Self {
        Self {
            tcp_port_table: Mutex::new(HashMap::new()),
        }
    }
}

pub const DEFAULT_LOCAL_PORT_RANGE: u32 = (32768u32 << 16) | 60999u32;

impl PortManager {
    pub fn local_port_range() -> (u16, u16) {
        ProcessManager::current_netns().local_port_range()
    }

    pub fn set_local_port_range(min: u16, max: u16) -> Result<(), SystemError> {
        ProcessManager::current_netns().set_local_port_range(min, max)
    }

    /// @brief 自动分配一个相对应协议中未被使用的PORT，如果动态端口均已被占用，返回错误码 EADDRINUSE
    pub fn get_ephemeral_port(&self, socket_type: Types) -> Result<u16, SystemError> {
        // TODO: selects non-conflict high port
        static EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(0);
        let (min, max) = Self::local_port_range();
        let range = (max - min) as u32 + 1;
        if range == 0 {
            return Err(SystemError::EINVAL);
        }
        let current = EPHEMERAL_PORT.load(Ordering::Relaxed);
        if current < min || current > max {
            let initial = min + (rand() % range as usize) as u16;
            EPHEMERAL_PORT.store(initial, Ordering::Relaxed);
        }

        let mut remaining = range;
        while remaining > 0 {
            let old = EPHEMERAL_PORT
                .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |cur| {
                    let cur = if cur < min || cur > max { min } else { cur };
                    Some(if cur >= max { min } else { cur + 1 })
                })
                .unwrap_or_else(|cur| cur);
            let port = if old < min || old >= max {
                min
            } else {
                old + 1
            };

            // 使用 ListenTable 检查端口是否被占用
            match socket_type {
                Tcp => {
                    let guard = self.tcp_port_table.lock();
                    if guard.get(&port).is_none() {
                        drop(guard);
                        return Ok(port);
                    }
                }
                _ => panic!("{:?} cann't get a port", socket_type),
            }
            remaining -= 1;
        }
        return Err(SystemError::EADDRINUSE);
    }

    #[inline]
    pub fn bind_ephemeral_port(&self, socket_type: Types) -> Result<u16, SystemError> {
        let (min, max) = Self::local_port_range();
        let range = (max - min) as u32 + 1;
        if range == 0 {
            return Err(SystemError::EINVAL);
        }
        let mut remaining = range;
        while remaining > 0 {
            let port = self.get_ephemeral_port(socket_type)?;
            match self.bind_port(socket_type, port) {
                Ok(()) => return Ok(port),
                Err(SystemError::EADDRINUSE) => {
                    // Race: another thread grabbed the port after we checked.
                    remaining -= 1;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(SystemError::EADDRINUSE)
    }

    /// @brief 检测给定端口是否已被占用，如果未被占用则在 TCP 对应的表中记录
    ///
    pub fn bind_port(&self, socket_type: Types, port: u16) -> Result<(), SystemError> {
        if port > 0 && socket_type == Tcp {
            let mut guard = self.tcp_port_table.lock();
            if guard.get(&port).is_some() {
                return Err(SystemError::EADDRINUSE);
            }
            guard.insert(port, ProcessManager::current_pid());
        }
        return Ok(());
    }

    /// @brief 在对应的端口记录表中将端口和 socket 解绑
    /// should call this function when socket is closed or aborted
    pub fn unbind_port(&self, socket_type: Types, port: u16) {
        if socket_type == Tcp {
            self.tcp_port_table.lock().remove(&port);
        };
    }
}
