use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::libs::rwlock::RwLock;
use system_error::SystemError;

use super::addr::VsockEndpoint;

/// transport 层上报到 vsock 语义层的事件集合。
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct VsockTransportEvent {
    pub local: VsockEndpoint,
    pub peer: VsockEndpoint,
    pub kind: VsockTransportEventKind,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum VsockTransportEventKind {
    /// 对端发起连接请求（对应 virtio REQUEST）。
    Request,
    /// 对端确认连接建立（对应 virtio RESPONSE）。
    Response,
    /// 对端发送数据（对应 virtio RW）。
    Rw { data: Vec<u8> },
    /// 对端半关闭/全关闭（对应 virtio SHUTDOWN）。
    Shutdown {
        /// 对端关闭发送方向：本端可读到 EOF。
        send_shutdown: bool,
        /// 对端关闭接收方向：本端发送应返回 EPIPE。
        recv_shutdown: bool,
    },
    /// 对端复位连接（对应 virtio RST）。
    Rst,
    /// 对端更新 credit。
    CreditUpdate,
    /// 对端请求 credit 更新。
    CreditRequest,
}

/// 全局传输后端可用性状态。
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum VsockTransportState {
    /// 未注册传输后端，或设备尚未就绪。
    Unavailable,
    /// 后端可正常收发。
    Ready,
    /// 后端进入不可恢复失败态。
    Failed,
}

enum GlobalVsockTransport {
    Unavailable,
    Ready {
        backend: Arc<dyn VsockTransport>,
    },
    Failed {
        backend: Option<Arc<dyn VsockTransport>>,
        error: SystemError,
    },
}

/// 非本地（跨 CID）vsock 流量的传输后端抽象。
///
/// `VsockStreamSocket` 对同 CID 的本地连接会直接走内存队列；
/// 对远端 CID 则通过该 trait 转发到具体设备/驱动实现。
pub trait VsockTransport: Send + Sync {
    /// 返回后端暴露的本地 CID。
    fn local_cid(&self) -> u32;

    #[allow(dead_code)]
    /// 后端是否可用。
    ///
    /// 默认返回 `true`，具体后端可覆盖。
    fn is_ready(&self) -> bool {
        true
    }

    /// 发起远端连接。
    ///
    /// # 参数
    /// - `_local`: 本地端点
    /// - `_peer`: 远端端点
    ///
    /// # 返回
    /// - 默认返回 `ENODEV`（表示未实现）
    fn connect(&self, _local: VsockEndpoint, _peer: VsockEndpoint) -> Result<(), SystemError> {
        Err(SystemError::ENODEV)
    }

    /// 让后端开始在本地端口上接收远端连接请求。
    ///
    /// # 返回
    /// - 默认返回 `ENODEV`（表示未实现）
    fn listen(&self, _local: VsockEndpoint) -> Result<(), SystemError> {
        Err(SystemError::ENODEV)
    }

    /// 停止在本地端口上接收远端连接请求。
    ///
    /// # 返回
    /// - 默认返回 `Ok(())`（后端可选实现）
    fn unlisten(&self, _local: VsockEndpoint) -> Result<(), SystemError> {
        Ok(())
    }

    /// 发送数据到远端。
    ///
    /// # 参数
    /// - `_local`: 本地端点
    /// - `_peer`: 远端端点
    /// - `_buffer`: 待发送数据
    ///
    /// # 返回
    /// - 默认返回 `ENODEV`（表示未实现）
    fn send(
        &self,
        _local: VsockEndpoint,
        _peer: VsockEndpoint,
        _buffer: &[u8],
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENODEV)
    }

    /// 对远端连接执行 shutdown 协商。
    ///
    /// # 参数
    /// - `_local`: 本地端点
    /// - `_peer`: 远端端点
    /// - `_send_shutdown`: 是否关闭发送方向
    /// - `_recv_shutdown`: 是否关闭接收方向
    ///
    /// # 返回
    /// - 默认返回 `Ok(())`，表示不强制要求后端实现
    fn shutdown(
        &self,
        _local: VsockEndpoint,
        _peer: VsockEndpoint,
        _send_shutdown: bool,
        _recv_shutdown: bool,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    /// 主动复位连接（RST）。
    ///
    /// # 返回
    /// - 默认返回 `ENODEV`（表示未实现）
    fn reset(&self, _local: VsockEndpoint, _peer: VsockEndpoint) -> Result<(), SystemError> {
        Err(SystemError::ENODEV)
    }

    /// 拉取设备事件并转换为统一 transport 事件。
    ///
    /// # 返回
    /// - 默认返回空列表（无事件）
    fn poll_events(&self) -> Result<Vec<VsockTransportEvent>, SystemError> {
        Ok(Vec::new())
    }
}

lazy_static! {
    static ref GLOBAL_VSOCK_TRANSPORT: RwLock<GlobalVsockTransport> =
        RwLock::new(GlobalVsockTransport::Unavailable);
}
static LOCAL_CID_FALLBACK_LOGGED: AtomicBool = AtomicBool::new(false);

fn ready_backend() -> Result<Arc<dyn VsockTransport>, SystemError> {
    let guard = GLOBAL_VSOCK_TRANSPORT.read();
    match &*guard {
        GlobalVsockTransport::Ready { backend } if backend.is_ready() => Ok(backend.clone()),
        _ => Err(SystemError::ENODEV),
    }
}

#[allow(dead_code)]
/// 注册（或覆盖）全局传输后端。
///
/// # 参数
/// - `transport`: 传输后端对象
pub fn register_transport(transport: Arc<dyn VsockTransport>) {
    let mut guard = GLOBAL_VSOCK_TRANSPORT.write();
    *guard = GlobalVsockTransport::Ready { backend: transport };
    LOCAL_CID_FALLBACK_LOGGED.store(false, Ordering::Release);
}

/// 将 transport 状态迁移为 `Failed`。
///
/// # 返回
/// - `true`: 状态首次进入 `Failed`
/// - `false`: 已处于 `Failed`
pub fn mark_transport_failed(error: SystemError) -> bool {
    let mut guard = GLOBAL_VSOCK_TRANSPORT.write();
    if matches!(&*guard, GlobalVsockTransport::Failed { .. }) {
        return false;
    }

    let backend = match &*guard {
        GlobalVsockTransport::Ready { backend } => Some(backend.clone()),
        GlobalVsockTransport::Unavailable => None,
        GlobalVsockTransport::Failed { backend, .. } => backend.clone(),
    };

    *guard = GlobalVsockTransport::Failed { backend, error };
    LOCAL_CID_FALLBACK_LOGGED.store(false, Ordering::Release);
    true
}

/// 将 transport 状态迁移为 `Unavailable` 并清理后端。
#[allow(dead_code)]
pub fn mark_transport_unavailable() {
    let mut guard = GLOBAL_VSOCK_TRANSPORT.write();
    *guard = GlobalVsockTransport::Unavailable;
    LOCAL_CID_FALLBACK_LOGGED.store(false, Ordering::Release);
}

/// 获取当前 transport 状态。
#[allow(dead_code)]
pub fn transport_state() -> VsockTransportState {
    match &*GLOBAL_VSOCK_TRANSPORT.read() {
        GlobalVsockTransport::Unavailable => VsockTransportState::Unavailable,
        GlobalVsockTransport::Ready { .. } => VsockTransportState::Ready,
        GlobalVsockTransport::Failed { .. } => VsockTransportState::Failed,
    }
}

/// 获取最近一次 transport 失败错误码（若有）。
#[allow(dead_code)]
pub fn transport_last_error() -> Option<SystemError> {
    match &*GLOBAL_VSOCK_TRANSPORT.read() {
        GlobalVsockTransport::Failed { error, .. } => Some(error.clone()),
        _ => None,
    }
}

#[allow(dead_code)]
/// 查询传输后端是否已注册且处于 ready 状态。
///
/// # 返回
/// - `true`: 已注册且 `is_ready() == true`
/// - `false`: 未注册或后端未就绪
pub fn transport_ready() -> bool {
    ready_backend().is_ok()
}

/// 获取当前本地 CID。
///
/// # 返回
/// - 已注册后端时返回 `transport.local_cid()`
/// - 未注册时回退到 `VMADDR_CID_LOCAL`
pub fn transport_local_cid() -> u32 {
    if let Ok(transport) = ready_backend() {
        LOCAL_CID_FALLBACK_LOGGED.store(false, Ordering::Release);
        return transport.local_cid();
    }

    if LOCAL_CID_FALLBACK_LOGGED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        log::warn!("vsock: no ready transport, fallback local cid to VMADDR_CID_LOCAL");
    }

    super::addr::VMADDR_CID_LOCAL
}

/// 调用传输后端建立远端连接。
///
/// # 参数
/// - `local`: 本地端点
/// - `peer`: 远端端点
///
/// # 返回
/// - 未注册后端时返回 `ENODEV`
pub fn transport_connect(local: VsockEndpoint, peer: VsockEndpoint) -> Result<(), SystemError> {
    let transport = ready_backend()?;
    transport.connect(local, peer)
}

/// 调用传输后端在本地端口开启监听。
///
/// # 返回
/// - 未注册后端时返回 `ENODEV`
pub fn transport_listen(local: VsockEndpoint) -> Result<(), SystemError> {
    ready_backend()?.listen(local)
}

#[allow(dead_code)]
/// 调用传输后端停止本地端口监听。
///
/// # 行为
/// - 未注册后端时静默成功
pub fn transport_unlisten(local: VsockEndpoint) -> Result<(), SystemError> {
    if let Ok(transport) = ready_backend() {
        transport.unlisten(local)?;
    }
    Ok(())
}

/// 调用传输后端发送数据。
///
/// # 参数
/// - `local`: 本地端点
/// - `peer`: 远端端点
/// - `buffer`: 待发送数据
///
/// # 返回
/// - 未注册后端时返回 `ENODEV`
pub fn transport_send(
    local: VsockEndpoint,
    peer: VsockEndpoint,
    buffer: &[u8],
) -> Result<usize, SystemError> {
    ready_backend()?.send(local, peer, buffer)
}

/// 调用传输后端执行 shutdown。
///
/// # 参数
/// - `local`: 本地端点
/// - `peer`: 远端端点
/// - `send_shutdown`: 关闭发送方向
/// - `recv_shutdown`: 关闭接收方向
pub fn transport_shutdown(
    local: VsockEndpoint,
    peer: VsockEndpoint,
    send_shutdown: bool,
    recv_shutdown: bool,
) -> Result<(), SystemError> {
    ready_backend()?.shutdown(local, peer, send_shutdown, recv_shutdown)
}

#[allow(dead_code)]
/// 调用传输后端发送复位。
///
/// # 返回
/// - 未注册后端时返回 `ENODEV`
pub fn transport_reset(local: VsockEndpoint, peer: VsockEndpoint) -> Result<(), SystemError> {
    ready_backend()?.reset(local, peer)
}

#[allow(dead_code)]
/// 拉取 transport 事件。
///
/// # 行为
/// - transport 非 `Ready` 时返回 `ENODEV`
pub fn transport_poll_events() -> Result<Vec<VsockTransportEvent>, SystemError> {
    ready_backend()?.poll_events()
}
