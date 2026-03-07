use alloc::sync::Arc;

use super::space::VsockSpace;

lazy_static! {
    // 所有 AF_VSOCK 套接字共享同一个全局空间。
    static ref GLOBAL_VSOCK_SPACE: Arc<VsockSpace> = VsockSpace::new();
}

/// 获取全局 `VsockSpace`。
///
/// # 返回
/// - `Arc<VsockSpace>`：全局注册表的共享引用
///
/// # 行为
/// - 每次返回同一个全局对象的克隆引用（增加强引用计数）
pub fn global_vsock_space() -> Arc<VsockSpace> {
    GLOBAL_VSOCK_SPACE.clone()
}
