//! AF_VSOCK 的流式套接字入口。
//!
//! 本模块负责组合以下组件：
//! - 地址定义（`addr`）
//! - 全局空间注册表（`global` + `space`）
//! - 流式语义实现（`stream`）
//! - 远端传输后端抽象（`transport`）

pub mod addr;
mod global;
pub mod space;
pub mod stream;
pub mod transport;

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use system_error::SystemError;

use crate::libs::wait_queue::WaitQueue;
use crate::net::socket::{Socket, PSOCK};
use crate::process::kthread::{KernelThreadClosure, KernelThreadMechanism};
use crate::process::namespace::net_namespace::INIT_NET_NAMESPACE;
use crate::process::ProcessManager;
use crate::time::Duration;

pub use stream::VsockStreamSocket;

#[allow(unused_imports)]
pub use addr::{ConnectionId, VsockEndpoint, VMADDR_CID_ANY, VMADDR_CID_HOST, VMADDR_CID_LOCAL};
#[allow(unused_imports)]
pub use global::global_vsock_space;
#[allow(unused_imports)]
pub use space::VsockSpace;
#[allow(unused_imports)]
pub use transport::{
    mark_transport_failed, register_transport, transport_local_cid, transport_ready,
    VsockTransport, VsockTransportEvent,
};

const VSOCK_EVENT_WORKER_IDLE_SLEEP: Duration = Duration::from_millis(1);
const VSOCK_EVENT_WORKER_ERROR_SLEEP: Duration = Duration::from_millis(10);

static VSOCK_EVENT_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static VSOCK_EVENT_WORKER_SLEEP_QUEUE: WaitQueue = WaitQueue::default();

/// 让 vsock 事件线程按给定超时时间休眠。
///
/// # 参数
/// - `timeout`: 睡眠时长
///
/// # 行为
/// - 通过 `WaitQueue` 的超时等待实现退避
fn vsock_event_worker_sleep(timeout: Duration) {
    let _ =
        VSOCK_EVENT_WORKER_SLEEP_QUEUE.wait_event_uninterruptible_timeout(|| false, Some(timeout));
}

/// vsock transport 事件轮询线程主循环。
///
/// # 返回
/// - 线程退出码（固定为 `0`）
///
/// # 行为
/// - 持续轮询 `transport_poll_events`
/// - 空闲或错误时退避睡眠，避免 busy loop
/// - 有事件时交给 `stream::dispatch_transport_events`
fn vsock_event_worker() -> i32 {
    loop {
        if KernelThreadMechanism::should_stop(&ProcessManager::current_pcb()) {
            break;
        }

        match transport::transport_poll_events() {
            Ok(events) => {
                if events.is_empty() {
                    vsock_event_worker_sleep(VSOCK_EVENT_WORKER_IDLE_SLEEP);
                    continue;
                }
                stream::dispatch_transport_events(events);
            }
            Err(SystemError::ENODEV) => {
                if mark_transport_failed(SystemError::ENODEV) {
                    stream::handle_transport_fatal_error(SystemError::ENODEV);
                }
                // 设备暂不可用时退避，避免空转。
                vsock_event_worker_sleep(VSOCK_EVENT_WORKER_ERROR_SLEEP);
            }
            Err(error) => {
                log::warn!("vsock event worker poll failed: {:?}", error);
                if mark_transport_failed(error.clone()) {
                    stream::handle_transport_fatal_error(error.clone());
                }
                vsock_event_worker_sleep(VSOCK_EVENT_WORKER_ERROR_SLEEP);
            }
        }
    }

    0
}

/// 确保 vsock transport 事件线程仅启动一次。
///
/// # 行为
/// - 通过原子标志保证幂等
/// - 线程创建失败会回滚启动标志
pub fn ensure_transport_event_worker_started() {
    if VSOCK_EVENT_WORKER_STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let closure = Box::new(vsock_event_worker);
    if KernelThreadMechanism::create_and_run(
        KernelThreadClosure::EmptyClosure((closure, ())),
        "vsock-event".to_string(),
    )
    .is_none()
    {
        VSOCK_EVENT_WORKER_STARTED.store(false, Ordering::Release);
        log::error!("failed to start vsock event worker");
        return;
    }

    log::info!("vsock event worker started");
}

/// 创建 AF_VSOCK 套接字对象。
///
/// # 参数
/// - `socket_type`: 套接字类型，当前仅支持 `SOCK_STREAM`
/// - `protocol`: 协议号，当前仅支持 0（默认 vsock 协议）
/// - `is_nonblock`: 初始是否为非阻塞模式
///
/// # 行为
/// - 非 `SOCK_STREAM` 返回 `ESOCKTNOSUPPORT`
/// - `protocol != 0` 返回 `EPROTONOSUPPORT`
/// - 非 init netns 返回 `EAFNOSUPPORT`
/// - 成功时返回 `VsockStreamSocket`
pub fn create_vsock_socket(
    socket_type: PSOCK,
    protocol: u32,
    is_nonblock: bool,
) -> Result<Arc<dyn Socket>, SystemError> {
    // 当前只支持流式语义PSOCK::Stream。
    if socket_type != PSOCK::Stream {
        return Err(SystemError::ESOCKTNOSUPPORT);
    }

    // Linux 下 protocol=0 表示默认 vsock 传输。
    if protocol != 0 {
        return Err(SystemError::EPROTONOSUPPORT);
    }

    // 当前仅在 init netns 启用 AF_VSOCK，避免 netns 路由歧义。
    let current_netns = ProcessManager::current_netns();
    if !Arc::ptr_eq(&current_netns, &INIT_NET_NAMESPACE) {
        return Err(SystemError::EAFNOSUPPORT);
    }

    Ok(VsockStreamSocket::new(is_nonblock)? as Arc<dyn Socket>)
}
