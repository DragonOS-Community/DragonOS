//! TCP close(2) 语义辅助：延迟回收 smoltcp TCP socket，贴近 Linux 行为且不修改 smoltcp。
//!
//! Linux 语义简述：
//! - close(fd) 仅释放文件描述符引用，内核仍会让 TCP 状态机继续运行（发送 FIN/重传/进入 TIME_WAIT 等）；
//! - 当协议状态机到达 Closed 且引用释放后，`struct sock` 才会真正销毁。
//!
//! DragonOS/smoltcp 适配点：
//! - smoltcp 的 `SocketHandle` 必须留在 `SocketSet` 里才能继续推进状态机；
//! - 但 close(fd) 后包裹该 handle 的 `TcpSocket` 可能立刻 drop，因此需要一个
//!   “独立于 TcpSocket 生命周期”的回收队列来保存 handle，等状态到 Closed 再 remove。

use alloc::sync::Weak;
use alloc::vec::Vec;

use crate::libs::mutex::Mutex;
use crate::net::socket::inet::common::PortManager;
use crate::net::socket::inet::InetSocket;
use crate::net::socket::inet::Types;

#[derive(Debug, Clone)]
struct ClosingTcpSocket {
    handle: smoltcp::iface::SocketHandle,
    local_port: u16,
    /// 对应的内核 TcpSocket（或其 trait object）。
    ///
    /// 目的：避免并发窗口——当 handle 已从 SocketSet remove 后，仍有在途引用调用
    /// update_events()/poll/notify 并访问该 handle，会触发 smoltcp panic。
    ///
    /// 只有当 Weak 无法 upgrade（说明 socket 对象已彻底释放）时，才允许回收 handle。
    sock: Weak<dyn InetSocket>,
}

/// 延迟回收 TCP sockets：close(fd) 后不立刻从 SocketSet 移除，等状态机 Closed 再回收。
#[derive(Debug)]
pub struct TcpCloseDefer {
    closing: Mutex<Vec<ClosingTcpSocket>>,
}

impl TcpCloseDefer {
    pub fn new() -> Self {
        Self {
            closing: Mutex::new(Vec::new()),
        }
    }

    #[inline]
    pub fn defer_tcp_close(
        &self,
        handle: smoltcp::iface::SocketHandle,
        local_port: u16,
        sock: Weak<dyn InetSocket>,
    ) {
        let mut guard = self.closing.lock();
        guard.push(ClosingTcpSocket {
            handle,
            local_port,
            sock,
        });
    }

    /// 在持有 `SocketSet` 锁的前提下，回收已进入 Closed 的 TCP sockets。
    ///
    /// 重要：
    /// - 锁顺序必须保持为：`SocketSet` -> `TcpCloseDefer::closing`，避免与 close 路径反转。
    pub fn reap_closed(
        &self,
        sockets: &mut smoltcp::iface::SocketSet<'static>,
        port_manager: &PortManager,
    ) {
        let mut closing = self.closing.lock();
        if closing.is_empty() {
            return;
        }
        let mut i = 0;
        while i < closing.len() {
            let ClosingTcpSocket {
                handle,
                local_port,
                ref sock,
            } = closing[i];
            let state = sockets.get::<smoltcp::socket::tcp::Socket>(handle).state();
            if matches!(state, smoltcp::socket::tcp::State::Closed) {
                // 关键：若仍存在在途引用（Weak 可 upgrade），则暂不回收 handle。
                // 否则在 handle remove 后，update_events()/poll/notify 仍可能访问该 handle，
                // 触发 smoltcp 的 "handle does not refer to a valid socket" panic。
                if sock.upgrade().is_some() {
                    i += 1;
                    continue;
                }
                sockets.remove(handle);
                port_manager.unbind_port(Types::Tcp, local_port);
                closing.swap_remove(i);
                continue;
            }
            i += 1;
        }
    }
}
