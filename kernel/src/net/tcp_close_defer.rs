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
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::libs::mutex::Mutex;
use crate::net::socket::inet::InetSocket;

const TCP_ORPHAN_MAX_LIFETIME_SECS: u64 = 60;
const TCP_CLOSE_REAP_BUDGET: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredTcpCloseKind {
    GracefulFin,
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredTcpCloseReason {
    NormalClose,
    ZeroLinger,
    UnreadDataOnClose,
    PostCloseData,
    OrphanTimeout,
    ConnectingClose,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TcpCloseDeferStats {
    pub current: usize,
    pub graceful_deferred: usize,
    pub reset_deferred: usize,
    pub unread_abort: usize,
    pub zero_linger_abort: usize,
    pub post_close_data_abort: usize,
    pub orphan_timeout_abort: usize,
    pub closed_reaped: usize,
    pub reset_pending_dropped: usize,
}

#[derive(Debug, Clone)]
pub struct DeferredTcpCloseRequest {
    pub handle: smoltcp::iface::SocketHandle,
    pub local_port: u16,
    pub sock: Weak<dyn InetSocket>,
    pub initial_state: smoltcp::socket::tcp::State,
    pub kind: DeferredTcpCloseKind,
    pub reason: DeferredTcpCloseReason,
    pub abort_on_post_close_data: bool,
}

#[derive(Debug, Clone)]
struct ClosingTcpSocket {
    handle: smoltcp::iface::SocketHandle,
    _local_port: u16,
    /// 对应的内核 TcpSocket（或其 trait object）。
    ///
    /// 目的：避免并发窗口——当 handle 已从 SocketSet remove 后，仍有在途引用调用
    /// update_events()/poll/notify 并访问该 handle，会触发 smoltcp panic。
    ///
    /// 只有当 Weak 无法 upgrade（说明 socket 对象已彻底释放）时，才允许回收 handle。
    sock: Weak<dyn InetSocket>,
    deferred_at: smoltcp::time::Instant,
    last_state: smoltcp::socket::tcp::State,
    _state_since: smoltcp::time::Instant,
    _kind: DeferredTcpCloseKind,
    _reason: DeferredTcpCloseReason,
    abort_on_post_close_data: bool,
}

/// 延迟回收 TCP sockets：close(fd) 后不立刻从 SocketSet 移除，等状态机 Closed 再回收。
#[derive(Debug)]
pub struct TcpCloseDefer {
    closing: Mutex<Vec<ClosingTcpSocket>>,
    reap_cursor: AtomicUsize,
    stats: Mutex<TcpCloseDeferStats>,
}

impl TcpCloseDefer {
    pub fn new() -> Self {
        Self {
            closing: Mutex::new(Vec::new()),
            reap_cursor: AtomicUsize::new(0),
            stats: Mutex::new(TcpCloseDeferStats::default()),
        }
    }

    #[inline]
    pub fn defer_tcp_close(&self, now: smoltcp::time::Instant, request: DeferredTcpCloseRequest) {
        let mut guard = self.closing.lock();
        guard.push(ClosingTcpSocket {
            handle: request.handle,
            _local_port: request.local_port,
            sock: request.sock,
            deferred_at: now,
            last_state: request.initial_state,
            _state_since: now,
            _kind: request.kind,
            _reason: request.reason,
            abort_on_post_close_data: request.abort_on_post_close_data,
        });
        drop(guard);

        let mut stats = self.stats.lock();
        stats.current += 1;
        match request.kind {
            DeferredTcpCloseKind::GracefulFin => stats.graceful_deferred += 1,
            DeferredTcpCloseKind::Reset => stats.reset_deferred += 1,
        }
        match request.reason {
            DeferredTcpCloseReason::UnreadDataOnClose => stats.unread_abort += 1,
            DeferredTcpCloseReason::ZeroLinger => stats.zero_linger_abort += 1,
            _ => {}
        }
    }

    #[inline]
    #[allow(dead_code)]
    pub fn stats(&self) -> TcpCloseDeferStats {
        let current = self.closing.lock().len();
        let mut stats = *self.stats.lock();
        stats.current = current;
        stats
    }

    /// 在持有 `SocketSet` 锁的前提下，回收已进入 Closed 的 TCP sockets。
    ///
    /// 重要：
    /// - 锁顺序必须保持为：`SocketSet` -> `TcpCloseDefer::closing`，避免与 close 路径反转。
    pub fn reap_closed(
        &self,
        now: smoltcp::time::Instant,
        sockets: &mut smoltcp::iface::SocketSet<'static>,
    ) {
        let mut closing = self.closing.lock();
        if closing.is_empty() {
            return;
        }
        let max_scan = closing.len().min(TCP_CLOSE_REAP_BUDGET);
        let mut scanned = 0usize;
        let mut i = self
            .reap_cursor
            .load(Ordering::Relaxed)
            .min(closing.len() - 1);
        while scanned < max_scan && !closing.is_empty() {
            if i >= closing.len() {
                i = 0;
            }
            scanned += 1;

            let handle = closing[i].handle;
            let state = sockets.get::<smoltcp::socket::tcp::Socket>(handle).state();
            if state != closing[i].last_state {
                closing[i].last_state = state;
                closing[i]._state_since = now;
            }

            let age = now - closing[i].deferred_at;
            let orphan_timed_out =
                age >= smoltcp::time::Duration::from_secs(TCP_ORPHAN_MAX_LIFETIME_SECS);

            let should_abort_post_close_data = closing[i].abort_on_post_close_data
                && !matches!(state, smoltcp::socket::tcp::State::Closed)
                && sockets
                    .get::<smoltcp::socket::tcp::Socket>(handle)
                    .recv_queue()
                    > 0;

            if should_abort_post_close_data
                || (orphan_timed_out && !matches!(state, smoltcp::socket::tcp::State::Closed))
            {
                sockets
                    .get_mut::<smoltcp::socket::tcp::Socket>(handle)
                    .abort();
                closing[i]._kind = DeferredTcpCloseKind::Reset;
                closing[i]._reason = if should_abort_post_close_data {
                    DeferredTcpCloseReason::PostCloseData
                } else {
                    DeferredTcpCloseReason::OrphanTimeout
                };
                closing[i].abort_on_post_close_data = false;

                let mut stats = self.stats.lock();
                if should_abort_post_close_data {
                    stats.post_close_data_abort += 1;
                } else {
                    stats.orphan_timeout_abort += 1;
                }
                i += 1;
                continue;
            }

            // Linux moves TIME-WAIT into a lightweight inet_timewait_sock.  Keeping
            // fd-less smoltcp TIME-WAIT sockets in SocketSet makes every iface.poll()
            // scan historical closes, so reclaim the full socket once no TcpSocket
            // user can observe the handle any more.
            if matches!(
                state,
                smoltcp::socket::tcp::State::Closed | smoltcp::socket::tcp::State::TimeWait
            ) {
                let rst_pending = matches!(state, smoltcp::socket::tcp::State::Closed)
                    && sockets
                        .get::<smoltcp::socket::tcp::Socket>(handle)
                        .remote_endpoint()
                        .is_some();
                let is_reset_close = closing[i]._kind == DeferredTcpCloseKind::Reset;
                if rst_pending && !is_reset_close && !orphan_timed_out {
                    i += 1;
                    continue;
                }

                // 关键：若仍存在在途引用（Weak 可 upgrade），则暂不回收 handle。
                // 否则在 handle remove 后，update_events()/poll/notify 仍可能访问该 handle，
                // 触发 smoltcp 的 "handle does not refer to a valid socket" panic。
                if closing[i].sock.upgrade().is_some() {
                    i += 1;
                    continue;
                }
                sockets.remove(handle);
                closing.swap_remove(i);
                let mut stats = self.stats.lock();
                stats.closed_reaped += 1;
                if rst_pending {
                    stats.reset_pending_dropped += 1;
                }
                continue;
            }
            i += 1;
        }

        if closing.is_empty() {
            self.reap_cursor.store(0, Ordering::Relaxed);
        } else {
            self.reap_cursor.store(i % closing.len(), Ordering::Relaxed);
        }
    }
}
