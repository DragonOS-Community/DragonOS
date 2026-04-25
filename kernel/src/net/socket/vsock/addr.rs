/// Vsock 端点地址，由 `(cid, port)` 唯一标识。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct VsockEndpoint {
    pub cid: u32,
    pub port: u32,
}

impl VsockEndpoint {
    /// 构造一个端点地址。
    ///
    /// # 参数
    /// - `cid`: 上下文标识（Context ID）
    /// - `port`: 端口号
    ///
    /// # 返回
    /// - 新的 `VsockEndpoint`
    pub const fn new(cid: u32, port: u32) -> Self {
        Self { cid, port }
    }
}

/// 全局 vsock 空间中的连接键。
///
/// `local` 与 `peer` 是有方向的，因此客户端和服务端会使用镜像键。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ConnectionId {
    pub local: VsockEndpoint,
    pub peer: VsockEndpoint,
}

impl ConnectionId {
    /// 构造一个有方向的连接键。
    pub const fn new(local: VsockEndpoint, peer: VsockEndpoint) -> Self {
        Self { local, peer }
    }

    /// 返回镜像方向的连接键。
    pub const fn mirror(&self) -> Self {
        Self {
            local: self.peer,
            peer: self.local,
        }
    }
}

/// 虚拟机监控器（Hypervisor）保留 CID。
#[allow(dead_code)]
pub const VMADDR_CID_HYPERVISOR: u32 = 0;
/// 本地 CID 的别名。
pub const VMADDR_CID_LOCAL: u32 = 1;
/// 主机侧保留 CID。
#[allow(dead_code)]
pub const VMADDR_CID_HOST: u32 = 2;
/// 用户态 API 使用的通配 CID。
pub const VMADDR_CID_ANY: u32 = u32::MAX;

/// bind/connect 中使用的通配端口。
pub const VMADDR_PORT_ANY: u32 = u32::MAX;
