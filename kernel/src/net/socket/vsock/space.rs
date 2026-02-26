use alloc::collections::btree_map::Entry;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use crate::libs::mutex::Mutex;
use system_error::SystemError;

use super::addr::{ConnectionId, VsockEndpoint, VMADDR_PORT_ANY};
use super::stream::VsockStreamSocket;

const EPHEMERAL_PORT_MIN: u32 = 49152;
const EPHEMERAL_PORT_MAX: u32 = 65535;

#[derive(Debug)]
pub struct VsockSpace {
    // 保护所有可变全局注册表。
    inner: Mutex<VsockSpaceInner>,
}

#[derive(Debug)]
struct VsockSpaceInner {
    // 临时端口分配游标（轮询推进）。
    next_ephemeral: u32,
    // 端口引用计数。计数归零时端口才真正释放。
    port_refs: BTreeMap<u32, usize>,
    // 监听套接字表：key 为本地端点。
    listeners: BTreeMap<VsockEndpoint, Weak<VsockStreamSocket>>,
    // 连接中套接字表：key 为本地端点。
    connecting: BTreeMap<VsockEndpoint, Weak<VsockStreamSocket>>,
    // 已连接套接字表：key 为 (local, peer)。
    connected: BTreeMap<ConnectionId, Weak<VsockStreamSocket>>,
}

impl VsockSpace {
    /// 创建 `VsockSpace`。
    ///
    /// # 返回
    /// - 新的全局空间对象，内部包含空的端口/连接注册表
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VsockSpaceInner {
                next_ephemeral: EPHEMERAL_PORT_MIN,
                port_refs: BTreeMap::new(),
                listeners: BTreeMap::new(),
                connecting: BTreeMap::new(),
                connected: BTreeMap::new(),
            }),
        })
    }

    /// 分配一个临时端口（ephemeral port）。
    ///
    /// # 返回
    /// - `Ok(port)`：成功分配并持有一次端口引用
    /// - `Err(EADDRINUSE)`：端口区间已耗尽
    ///
    /// # 行为
    /// - 在 `[49152, 65535]` 范围内按轮询方式查找空闲端口
    /// - 找到后在 `port_refs` 中登记初始引用计数 1
    pub fn alloc_ephemeral_port(&self) -> Result<u32, SystemError> {
        let mut guard = self.inner.lock();
        // 从当前游标开始，最多扫描整个临时端口区间一次。
        for _ in EPHEMERAL_PORT_MIN..=EPHEMERAL_PORT_MAX {
            let port = guard.next_ephemeral;
            guard.next_ephemeral += 1;
            if guard.next_ephemeral > EPHEMERAL_PORT_MAX {
                guard.next_ephemeral = EPHEMERAL_PORT_MIN;
            }

            if let Entry::Vacant(entry) = guard.port_refs.entry(port) {
                entry.insert(1);
                return Ok(port);
            }
        }
        Err(SystemError::EADDRINUSE)
    }

    /// 独占保留指定端口。
    ///
    /// # 参数
    /// - `port`: 需要保留的明确端口
    ///
    /// # 行为
    /// - `VMADDR_PORT_ANY` 视为非法，返回 `EINVAL`
    /// - 若端口已被占用，返回 `EADDRINUSE`
    /// - 成功时写入 `port_refs[port] = 1`
    pub fn reserve_port(&self, port: u32) -> Result<(), SystemError> {
        if port == VMADDR_PORT_ANY {
            return Err(SystemError::EINVAL);
        }

        let mut guard = self.inner.lock();
        // 显式 bind(port) 需要独占该端口。
        match guard.port_refs.entry(port) {
            Entry::Vacant(entry) => {
                entry.insert(1);
                Ok(())
            }
            Entry::Occupied(_) => Err(SystemError::EADDRINUSE),
        }
    }

    /// 增加端口引用计数（共享持有）。
    ///
    /// # 参数
    /// - `port`: 端口号
    ///
    /// # 行为
    /// - 典型场景是监听套接字 accept 出子连接后复用同一端口
    /// - `VMADDR_PORT_ANY` 返回 `EINVAL`
    pub fn retain_port(&self, port: u32) -> Result<(), SystemError> {
        if port == VMADDR_PORT_ANY {
            return Err(SystemError::EINVAL);
        }

        let mut guard = self.inner.lock();
        // 共享持有端口，例如 accept 子连接继承监听端口。
        *guard.port_refs.entry(port).or_insert(0) += 1;
        Ok(())
    }

    /// 释放一次端口引用计数。
    ///
    /// # 参数
    /// - `port`: 端口号
    ///
    /// # 行为
    /// - 计数大于 1 时仅减 1
    /// - 计数降到 0 时移除端口记录
    pub fn release_port(&self, port: u32) {
        let mut guard = self.inner.lock();
        if let Some(ref_count) = guard.port_refs.get_mut(&port) {
            if *ref_count > 1 {
                *ref_count -= 1;
            } else {
                guard.port_refs.remove(&port);
            }
        }
    }

    /// 注册监听套接字。
    ///
    /// # 参数
    /// - `local`: 监听本地端点
    /// - `listener`: 监听 socket 的弱引用
    ///
    /// # 行为
    /// - 同一端点只能存在一个“活跃监听者”
    /// - 若是同一个 socket 的重复 listen，则幂等成功
    /// - 冲突时返回 `EADDRINUSE`
    pub fn register_listener(
        &self,
        local: VsockEndpoint,
        listener: Weak<VsockStreamSocket>,
    ) -> Result<(), SystemError> {
        let mut guard = self.inner.lock();
        // 同一端点只允许一个活跃监听者。
        if let Some(existing) = guard.listeners.get(&local).and_then(|weak| weak.upgrade()) {
            if let Some(new_listener) = listener.upgrade() {
                if Arc::ptr_eq(&existing, &new_listener) {
                    // 同一个 socket 重复 listen，按幂等处理。
                    guard.listeners.insert(local, listener);
                    return Ok(());
                }
            }
            return Err(SystemError::EADDRINUSE);
        }

        guard.listeners.insert(local, listener);
        Ok(())
    }

    /// 注销监听套接字。
    ///
    /// # 参数
    /// - `local`: 监听本地端点
    pub fn unregister_listener(&self, local: VsockEndpoint) {
        self.inner.lock().listeners.remove(&local);
    }

    /// 查找监听套接字。
    ///
    /// # 参数
    /// - `local`: 目标本地端点
    ///
    /// # 返回
    /// - `Some(listener)`：找到有效监听者
    /// - `None`：未找到或弱引用已失效
    ///
    /// # 行为
    /// - 若检测到失效弱引用，会顺带清理脏表项
    pub fn find_listener(&self, local: VsockEndpoint) -> Option<Arc<VsockStreamSocket>> {
        let mut guard = self.inner.lock();
        let listener = guard.listeners.get(&local).cloned();
        match listener {
            Some(weak) => match weak.upgrade() {
                Some(listener) => Some(listener),
                None => {
                    // 清理已经失效的弱引用表项。
                    guard.listeners.remove(&local);
                    None
                }
            },
            None => None,
        }
    }

    /// 注册“连接中”套接字。
    ///
    /// # 参数
    /// - `local`: 本地端点
    /// - `socket`: 套接字弱引用
    pub fn register_connecting(&self, local: VsockEndpoint, socket: Weak<VsockStreamSocket>) {
        self.inner.lock().connecting.insert(local, socket);
    }

    /// 注销“连接中”套接字。
    ///
    /// # 参数
    /// - `local`: 本地端点
    pub fn unregister_connecting(&self, local: VsockEndpoint) {
        self.inner.lock().connecting.remove(&local);
    }

    /// 查找“连接中”套接字。
    ///
    /// # 参数
    /// - `local`: 本地端点
    ///
    /// # 返回
    /// - `Some(socket)`：找到有效连接中 socket
    /// - `None`：未找到或弱引用已失效
    pub fn find_connecting(&self, local: VsockEndpoint) -> Option<Arc<VsockStreamSocket>> {
        let mut guard = self.inner.lock();
        let socket = guard.connecting.get(&local).cloned();
        match socket {
            Some(weak) => match weak.upgrade() {
                Some(socket) => Some(socket),
                None => {
                    guard.connecting.remove(&local);
                    None
                }
            },
            None => None,
        }
    }

    /// 注册“已连接”套接字。
    ///
    /// # 参数
    /// - `id`: 连接键（local, peer）
    /// - `socket`: 套接字弱引用
    pub fn register_connected(&self, id: ConnectionId, socket: Weak<VsockStreamSocket>) {
        self.inner.lock().connected.insert(id, socket);
    }

    /// 注销“已连接”套接字。
    ///
    /// # 参数
    /// - `id`: 连接键（local, peer）
    pub fn unregister_connected(&self, id: ConnectionId) {
        self.inner.lock().connected.remove(&id);
    }

    /// 查找“已连接”套接字。
    ///
    /// # 参数
    /// - `id`: 连接键（local, peer）
    ///
    /// # 返回
    /// - `Some(socket)`：找到有效已连接 socket
    /// - `None`：未找到或弱引用已失效
    pub fn find_connected(&self, id: ConnectionId) -> Option<Arc<VsockStreamSocket>> {
        let mut guard = self.inner.lock();
        let socket = guard.connected.get(&id).cloned();
        match socket {
            Some(weak) => match weak.upgrade() {
                Some(socket) => Some(socket),
                None => {
                    guard.connected.remove(&id);
                    None
                }
            },
            None => None,
        }
    }

    /// 摘取并清空全部“连接中”套接字。
    ///
    /// # 返回
    /// - `(local, socket)` 列表，仅包含仍然有效的强引用
    pub fn take_all_connecting(&self) -> Vec<(VsockEndpoint, Arc<VsockStreamSocket>)> {
        let mut guard = self.inner.lock();
        let connecting = core::mem::take(&mut guard.connecting);
        let mut sockets = Vec::new();
        for (local, weak) in connecting {
            if let Some(socket) = weak.upgrade() {
                sockets.push((local, socket));
            }
        }
        sockets
    }

    /// 摘取并清空全部“已连接”套接字。
    ///
    /// # 返回
    /// - `((local, peer), socket)` 列表，仅包含仍然有效的强引用
    pub fn take_all_connected(&self) -> Vec<(ConnectionId, Arc<VsockStreamSocket>)> {
        let mut guard = self.inner.lock();
        let connected = core::mem::take(&mut guard.connected);
        let mut sockets = Vec::new();
        for (id, weak) in connected {
            if let Some(socket) = weak.upgrade() {
                sockets.push((id, socket));
            }
        }
        sockets
    }
}
