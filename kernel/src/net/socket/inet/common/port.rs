use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use hashbrown::HashMap;
use smoltcp::wire::IpAddress;
use system_error::SystemError;

use crate::{
    arch::rand::rand,
    libs::mutex::Mutex,
    process::{ProcessManager, RawPid},
};

use super::Types::{self, *};

/// # TCP 和 UDP 的端口管理器。
/// 如果 TCP/UDP 的 socket 绑定了某个端口，它会在对应的表中记录，以检测端口冲突。
#[derive(Debug)]
pub struct PortManager {
    // TCP 端口记录表
    tcp_port_table: Mutex<HashMap<u16, RawPid>>,
    // UDP 端口记录表
    udp_port_table: Mutex<HashMap<u16, Vec<UdpPortBinding>>>,
}

impl Default for PortManager {
    fn default() -> Self {
        Self {
            tcp_port_table: Mutex::new(HashMap::new()),
            udp_port_table: Mutex::new(HashMap::new()),
        }
    }
}

const DEFAULT_LOCAL_PORT_RANGE: u32 = (32768u32 << 16) | 60999u32;
static LOCAL_PORT_RANGE: AtomicU32 = AtomicU32::new(DEFAULT_LOCAL_PORT_RANGE);

fn unpack_range(value: u32) -> (u16, u16) {
    ((value >> 16) as u16, (value & 0xffff) as u16)
}

impl PortManager {
    pub fn local_port_range() -> (u16, u16) {
        unpack_range(LOCAL_PORT_RANGE.load(Ordering::Relaxed))
    }

    pub fn set_local_port_range(min: u16, max: u16) -> Result<(), SystemError> {
        if min == 0 || max == 0 || min > max {
            return Err(SystemError::EINVAL);
        }
        let new_value = ((min as u32) << 16) | (max as u32);
        loop {
            let old_value = LOCAL_PORT_RANGE.load(Ordering::Relaxed);
            if old_value == new_value {
                return Ok(());
            }
            if LOCAL_PORT_RANGE
                .compare_exchange(old_value, new_value, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
        }
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
                Udp => {
                    let guard = self.udp_port_table.lock();
                    if guard.get(&port).is_none() {
                        drop(guard);
                        return Ok(port);
                    }
                }
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
        let port = self.get_ephemeral_port(socket_type)?;
        self.bind_port(socket_type, port)?;
        return Ok(port);
    }

    /// UDP: 绑定随机端口（支持 reuseaddr/reuseport 规则）
    pub fn bind_udp_ephemeral_port(
        &self,
        addr: IpAddress,
        reuseaddr: bool,
        reuseport: bool,
        bind_id: usize,
    ) -> Result<u16, SystemError> {
        let port = self.get_ephemeral_port(Types::Udp)?;
        self.bind_udp_port(port, addr, reuseaddr, reuseport, bind_id)?;
        Ok(port)
    }

    /// @brief 检测给定端口是否已被占用，如果未被占用则在 TCP 对应的表中记录
    ///
    /// UDP 复用逻辑请使用 `bind_udp_port`
    pub fn bind_port(&self, socket_type: Types, port: u16) -> Result<(), SystemError> {
        if port > 0 {
            match socket_type {
                Udp => {
                    let mut guard = self.udp_port_table.lock();
                    if guard.get(&port).is_some() {
                        return Err(SystemError::EADDRINUSE);
                    }
                    guard.insert(port, Vec::new());
                }
                Tcp => {
                    let mut guard = self.tcp_port_table.lock();
                    if guard.get(&port).is_some() {
                        return Err(SystemError::EADDRINUSE);
                    }
                    guard.insert(port, ProcessManager::current_pid());
                }
                _ => {}
            };
        }
        return Ok(());
    }

    /// @brief 在对应的端口记录表中将端口和 socket 解绑
    /// should call this function when socket is closed or aborted
    pub fn unbind_port(&self, socket_type: Types, port: u16) {
        match socket_type {
            Udp => {
                self.udp_port_table.lock().remove(&port);
            }
            Tcp => {
                self.tcp_port_table.lock().remove(&port);
            }
            _ => {}
        };
    }

    /// UDP: 绑定端口，支持 SO_REUSEADDR/SO_REUSEPORT
    pub fn bind_udp_port(
        &self,
        port: u16,
        addr: IpAddress,
        reuseaddr: bool,
        reuseport: bool,
        bind_id: usize,
    ) -> Result<(), SystemError> {
        if port == 0 {
            return Err(SystemError::EINVAL);
        }
        let mut guard = self.udp_port_table.lock();
        let bindings = guard.entry(port).or_default();
        for binding in bindings.iter() {
            if !udp_addrs_conflict(addr, binding.addr) {
                continue;
            }
            let share_ok = (reuseport && binding.reuseport) || (reuseaddr && binding.reuseaddr);
            if !share_ok {
                return Err(SystemError::EADDRINUSE);
            }
        }
        bindings.push(UdpPortBinding {
            addr,
            reuseaddr,
            reuseport,
            bind_id,
        });
        Ok(())
    }

    /// UDP: 解绑端口（按 bind_id）
    pub fn unbind_udp_port(&self, port: u16, bind_id: usize) {
        let mut guard = self.udp_port_table.lock();
        if let Some(list) = guard.get_mut(&port) {
            list.retain(|b| b.bind_id != bind_id);
            if list.is_empty() {
                guard.remove(&port);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct UdpPortBinding {
    addr: IpAddress,
    reuseaddr: bool,
    reuseport: bool,
    bind_id: usize,
}

#[inline]
fn udp_addrs_conflict(a: IpAddress, b: IpAddress) -> bool {
    if a.version() != b.version() {
        return false;
    }
    if a.is_unspecified() || b.is_unspecified() {
        return true;
    }
    a == b
}
