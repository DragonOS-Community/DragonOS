use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, Ordering};
use hashbrown::HashMap;
use smoltcp::wire::IpAddress;
use system_error::SystemError;

use crate::{
    arch::rand::rand,
    libs::mutex::Mutex,
    process::ProcessManager,
};

use super::Types::{self, *};

/// # TCP 和 UDP 的端口管理器。
/// 如果 TCP/UDP 的 socket 绑定了某个端口，它会在对应的表中记录，以检测端口冲突。
#[derive(Debug)]
pub struct PortManager {
    // TCP 端口记录表。一个端口可以有多条绑定记录（SO_REUSEPORT/SO_REUSEADDR 共享）。
    tcp_port_table: Mutex<HashMap<u16, Vec<TcpPortBinding>>>,
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
    pub fn bind_tcp_ephemeral_port(
        &self,
        addr: IpAddress,
        reuseaddr: bool,
        reuseport: bool,
        iface_nic_id: usize,
        handle: smoltcp::iface::SocketHandle,
    ) -> Result<u16, SystemError> {
        let (min, max) = Self::local_port_range();
        let range = (max - min) as u32 + 1;
        if range == 0 {
            return Err(SystemError::EINVAL);
        }
        let mut remaining = range;
        while remaining > 0 {
            let port = self.get_ephemeral_port(Types::Tcp)?;
            match self.bind_tcp_port(port, addr, reuseaddr, reuseport, iface_nic_id, handle) {
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

    /// UDP: 绑定随机端口（支持 reuseaddr/reuseport 规则）
    pub fn bind_udp_ephemeral_port(
        &self,
        addr: IpAddress,
        reuseaddr: bool,
        reuseport: bool,
        bind_id: usize,
    ) -> Result<u16, SystemError> {
        let (min, max) = Self::local_port_range();
        let range = (max - min) as u32 + 1;
        if range == 0 {
            return Err(SystemError::EINVAL);
        }
        let mut remaining = range;
        while remaining > 0 {
            let port = self.get_ephemeral_port(Types::Udp)?;
            match self.bind_udp_port(port, addr, reuseaddr, reuseport, bind_id) {
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

    /// TCP: 绑定端口，支持 SO_REUSEADDR/SO_REUSEPORT。
    ///
    /// 一条绑定记录以 `(iface_nic_id, handle)` 唯一标识（BoundInner 身份），
    /// 因此多个进程/多个 socket 可以共享同一端口而不需要调用方保存额外 id。
    pub fn bind_tcp_port(
        &self,
        port: u16,
        addr: IpAddress,
        reuseaddr: bool,
        reuseport: bool,
        iface_nic_id: usize,
        handle: smoltcp::iface::SocketHandle,
    ) -> Result<(), SystemError> {
        if port == 0 {
            return Err(SystemError::EINVAL);
        }
        let mut guard = self.tcp_port_table.lock();
        let bindings = guard.entry(port).or_default();
        for binding in bindings.iter() {
            if !addrs_conflict(addr, binding.addr) {
                continue;
            }
            let share_ok = (reuseport && binding.reuseport) || (reuseaddr && binding.reuseaddr);
            if !share_ok {
                return Err(SystemError::EADDRINUSE);
            }
        }
        bindings.push(TcpPortBinding {
            addr,
            reuseaddr,
            reuseport,
            iface_nic_id,
            handle,
        });
        Ok(())
    }

    /// TCP: 解绑端口（按 BoundInner 身份）
    pub fn unbind_tcp_port(&self, port: u16, iface_nic_id: usize, handle: smoltcp::iface::SocketHandle) {
        let mut guard = self.tcp_port_table.lock();
        if let Some(list) = guard.get_mut(&port) {
            list.retain(|b| b.iface_nic_id != iface_nic_id || b.handle != handle);
            if list.is_empty() {
                guard.remove(&port);
            }
        }
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
            if !addrs_conflict(addr, binding.addr) {
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

/// TCP 端口绑定记录。`(iface_nic_id, handle)` 是绑定的 BoundInner 身份。
#[derive(Debug, Clone)]
struct TcpPortBinding {
    addr: IpAddress,
    reuseaddr: bool,
    reuseport: bool,
    iface_nic_id: usize,
    handle: smoltcp::iface::SocketHandle,
}

#[inline]
fn addrs_conflict(a: IpAddress, b: IpAddress) -> bool {
    if a.version() != b.version() {
        return false;
    }
    if a.is_unspecified() || b.is_unspecified() {
        return true;
    }
    a == b
}
