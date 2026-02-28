use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::TryFrom;

use system_error::SystemError;
use virtio_drivers::device::socket::{
    DisconnectReason, SocketError, VirtIOSocket, VsockAddr, VsockConnectionManager, VsockEventType,
};
use virtio_drivers::Error as VirtioError;

use crate::driver::base::device::{Device, DeviceId};
use crate::driver::virtio::transport::VirtIOTransport;
use crate::driver::virtio::virtio_drivers_error_to_system_error;
use crate::driver::virtio::virtio_impl::HalImpl;
use crate::libs::mutex::Mutex;
use crate::net::socket::vsock::addr::VsockEndpoint;
use crate::net::socket::vsock::transport::{
    VsockTransport, VsockTransportEvent, VsockTransportEventKind,
};
use crate::net::socket::vsock::{ensure_transport_event_worker_started, register_transport};

struct UnsafeVsockManager(VsockConnectionManager<HalImpl, VirtIOTransport>);

// SAFETY: `VsockConnectionManager` 在 `virtio_drivers` 中没有实现 `Send`，
// 但本驱动保证它只会在 `VirtioVsockTransport::manager` 这把互斥锁保护下访问，
// 不会发生并发可变访问。且 `Mutex<T>: Sync where T: Send`，因此
// `Mutex<UnsafeVsockManager>` 可在线程间共享。
unsafe impl Send for UnsafeVsockManager {}

struct VirtioVsockTransport {
    manager: Mutex<UnsafeVsockManager>,
    guest_cid: u32,
}

impl VirtioVsockTransport {
    /// 构造 virtio-vsock 传输后端实例。
    ///
    /// # 参数
    /// - `manager`: 对底层 `VirtIOSocket` 的连接管理器
    /// - `guest_cid`: 当前虚机的本地 CID
    ///
    /// # 返回
    /// - 初始化完成的 `VirtioVsockTransport`
    fn new(manager: VsockConnectionManager<HalImpl, VirtIOTransport>, guest_cid: u32) -> Self {
        Self {
            manager: Mutex::new(UnsafeVsockManager(manager)),
            guest_cid,
        }
    }

    /// 将 DragonOS 的 vsock 端点转换为 `virtio_drivers` 地址格式。
    ///
    /// # 参数
    /// - `endpoint`: DragonOS 端点 `(cid, port)`
    ///
    /// # 返回
    /// - `virtio_drivers::VsockAddr`
    fn endpoint_to_vsock_addr(endpoint: VsockEndpoint) -> VsockAddr {
        VsockAddr {
            cid: endpoint.cid as u64,
            port: endpoint.port,
        }
    }

    /// 将 `virtio_drivers` 上报地址转换为 DragonOS 端点。
    ///
    /// # 参数
    /// - `addr`: `virtio_drivers` 的地址结构
    ///
    /// # 返回
    /// - `Some(VsockEndpoint)`: 转换成功
    /// - `None`: `cid` 超出 `u32` 范围
    fn event_addr_to_endpoint(addr: VsockAddr) -> Option<VsockEndpoint> {
        let cid = u32::try_from(addr.cid).ok()?;
        Some(VsockEndpoint {
            cid,
            port: addr.port,
        })
    }
}

impl VsockTransport for VirtioVsockTransport {
    fn local_cid(&self) -> u32 {
        self.guest_cid
    }

    fn is_ready(&self) -> bool {
        true
    }

    fn connect(&self, local: VsockEndpoint, peer: VsockEndpoint) -> Result<(), SystemError> {
        let mut manager = self.manager.lock();
        manager
            .0
            .connect(Self::endpoint_to_vsock_addr(peer), local.port)
            .map_err(map_vsock_error)
    }

    fn listen(&self, local: VsockEndpoint) -> Result<(), SystemError> {
        self.manager.lock().0.listen(local.port);
        Ok(())
    }

    fn unlisten(&self, local: VsockEndpoint) -> Result<(), SystemError> {
        self.manager.lock().0.unlisten(local.port);
        Ok(())
    }

    fn send(
        &self,
        local: VsockEndpoint,
        peer: VsockEndpoint,
        buffer: &[u8],
    ) -> Result<usize, SystemError> {
        let mut manager = self.manager.lock();
        manager
            .0
            .send(Self::endpoint_to_vsock_addr(peer), local.port, buffer)
            .map_err(map_vsock_error)?;
        Ok(buffer.len())
    }

    fn shutdown(
        &self,
        local: VsockEndpoint,
        peer: VsockEndpoint,
        send_shutdown: bool,
        recv_shutdown: bool,
    ) -> Result<(), SystemError> {
        if !send_shutdown && !recv_shutdown {
            return Ok(());
        }
        let mut manager = self.manager.lock();
        manager
            .0
            .shutdown(Self::endpoint_to_vsock_addr(peer), local.port)
            .map_err(map_vsock_error)
    }

    fn reset(&self, local: VsockEndpoint, peer: VsockEndpoint) -> Result<(), SystemError> {
        let mut manager = self.manager.lock();
        manager
            .0
            .force_close(Self::endpoint_to_vsock_addr(peer), local.port)
            .map_err(map_vsock_error)
    }

    fn poll_events(&self) -> Result<Vec<VsockTransportEvent>, SystemError> {
        let mut manager = self.manager.lock();
        let mut events = Vec::new();
        while let Some(event) = manager.0.poll().map_err(map_vsock_error)? {
            let Some(local) = Self::event_addr_to_endpoint(event.destination) else {
                log::warn!(
                    "drop vsock event with invalid destination cid={}",
                    event.destination.cid
                );
                continue;
            };
            let Some(peer) = Self::event_addr_to_endpoint(event.source) else {
                log::warn!(
                    "drop vsock event with invalid source cid={}",
                    event.source.cid
                );
                continue;
            };

            match event.event_type {
                VsockEventType::ConnectionRequest => {
                    events.push(VsockTransportEvent {
                        local,
                        peer,
                        kind: VsockTransportEventKind::Request,
                    });
                }
                VsockEventType::Connected => {
                    events.push(VsockTransportEvent {
                        local,
                        peer,
                        kind: VsockTransportEventKind::Response,
                    });
                }
                VsockEventType::Disconnected { reason } => match reason {
                    DisconnectReason::Reset => {
                        events.push(VsockTransportEvent {
                            local,
                            peer,
                            kind: VsockTransportEventKind::Rst,
                        });
                    }
                    DisconnectReason::Shutdown => {
                        events.push(VsockTransportEvent {
                            local,
                            peer,
                            kind: VsockTransportEventKind::Shutdown {
                                send_shutdown: true,
                                recv_shutdown: true,
                            },
                        });
                    }
                },
                VsockEventType::Received { length } => {
                    let mut data = vec![0u8; length];
                    match manager
                        .0
                        .recv(Self::endpoint_to_vsock_addr(peer), local.port, &mut data)
                    {
                        Ok(read_len) => {
                            data.truncate(read_len);
                            events.push(VsockTransportEvent {
                                local,
                                peer,
                                kind: VsockTransportEventKind::Rw { data },
                            });
                        }
                        Err(error) => {
                            log::warn!(
                                "vsock: recv failed for {:?}->{:?}: {:?}, skipping event",
                                peer,
                                local,
                                map_vsock_error(error)
                            );
                        }
                    }
                }
                VsockEventType::CreditRequest => {
                    events.push(VsockTransportEvent {
                        local,
                        peer,
                        kind: VsockTransportEventKind::CreditRequest,
                    });
                }
                VsockEventType::CreditUpdate => {
                    events.push(VsockTransportEvent {
                        local,
                        peer,
                        kind: VsockTransportEventKind::CreditUpdate,
                    });
                }
            }
        }
        Ok(events)
    }
}

/// 将 `virtio_drivers::SocketError` 映射为 DragonOS 的 `SystemError`。
///
/// # 参数
/// - `error`: vsock 协议层错误
///
/// # 返回
/// - 对应的 DragonOS 错误码
fn map_vsock_socket_error(error: SocketError) -> SystemError {
    match error {
        SocketError::ConnectionExists => SystemError::EALREADY,
        SocketError::ConnectionFailed => SystemError::ECONNREFUSED,
        SocketError::NotConnected => SystemError::ENOTCONN,
        SocketError::PeerSocketShutdown => SystemError::EPIPE,
        SocketError::NoResponseReceived => SystemError::ETIMEDOUT,
        SocketError::BufferTooShort => SystemError::EINVAL,
        SocketError::OutputBufferTooShort(_) => SystemError::EMSGSIZE,
        SocketError::BufferTooLong(_, _) => SystemError::EMSGSIZE,
        SocketError::UnknownOperation(_) => SystemError::EPROTO,
        SocketError::InvalidOperation => SystemError::EPROTO,
        SocketError::InvalidNumber => SystemError::EINVAL,
        SocketError::UnexpectedDataInPacket => SystemError::EPROTO,
        SocketError::InsufficientBufferSpaceInPeer => SystemError::EAGAIN_OR_EWOULDBLOCK,
        SocketError::RecycledWrongBuffer => SystemError::EIO,
    }
}

/// 将 `virtio_drivers::Error` 映射为 DragonOS 的 `SystemError`。
///
/// # 参数
/// - `error`: virtio 驱动返回错误
///
/// # 返回
/// - 对应的 DragonOS 错误码
fn map_vsock_error(error: VirtioError) -> SystemError {
    match error {
        VirtioError::SocketDeviceError(err) => map_vsock_socket_error(err),
        other => virtio_drivers_error_to_system_error(other),
    }
}

/// 初始化并注册 virtio-vsock 设备。
///
/// # 参数
/// - `transport`: virtio 传输层对象
/// - `dev_id`: 设备标识
/// - `_dev_parent`: 父设备（当前未使用）
///
/// # 行为
/// - 创建 `VirtIOSocket` 并读取 `guest_cid`
/// - 注册全局 `VsockTransport` 后端
/// - 启动 vsock 事件轮询线程（若未启动）
pub fn virtio_vsock(
    transport: VirtIOTransport,
    dev_id: Arc<DeviceId>,
    _dev_parent: Option<Arc<dyn Device>>,
) {
    log::info!("virtio-vsock: init start for device {:?}", dev_id.id());
    let socket = match VirtIOSocket::<HalImpl, VirtIOTransport>::new(transport) {
        Ok(socket) => socket,
        Err(err) => {
            log::error!(
                "virtio-vsock init failed for device {:?}: {:?}",
                dev_id.id(),
                err
            );
            return;
        }
    };

    let guest_cid = match u32::try_from(socket.guest_cid()) {
        Ok(cid) => cid,
        Err(_) => {
            log::error!(
                "virtio-vsock device {:?} reports guest cid={} out of u32 range",
                dev_id.id(),
                socket.guest_cid()
            );
            return;
        }
    };

    let manager = VsockConnectionManager::new(socket);
    let transport = Arc::new(VirtioVsockTransport::new(manager, guest_cid));
    log::info!(
        "virtio-vsock: registering transport for device {:?}, guest_cid={}",
        dev_id.id(),
        guest_cid
    );
    register_transport(transport as Arc<dyn VsockTransport>);
    ensure_transport_event_worker_started();
    log::info!(
        "virtio-vsock transport registered for device {:?}, guest_cid={}",
        dev_id.id(),
        guest_cid
    );
}
