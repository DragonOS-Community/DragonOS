use ida::IdAllocator;
use smoltcp::iface::SocketHandle;

use crate::libs::spinlock::SpinLock;

int_like!(KernelHandle, usize);

/// # socket的句柄管理组件
/// 它在smoltcp的SocketHandle上封装了一层，增加更多的功能。
/// 比如，在socket被关闭时，自动释放socket的资源，通知系统的其他组件。
#[derive(Debug, Hash, Eq, PartialEq, Clone, Copy)]
pub enum GlobalSocketHandle {
    Smoltcp(SocketHandle),
    Kernel(KernelHandle),
}

static KERNEL_HANDLE_IDA: SpinLock<IdAllocator> =
    SpinLock::new(IdAllocator::new(0, usize::MAX).unwrap());

impl GlobalSocketHandle {
    pub fn new_smoltcp_handle(handle: SocketHandle) -> Self {
        return Self::Smoltcp(handle);
    }

    pub fn new_kernel_handle() -> Self {
        return Self::Kernel(KernelHandle::new(KERNEL_HANDLE_IDA.lock().alloc().unwrap()));
    }

    pub fn smoltcp_handle(&self) -> Option<SocketHandle> {
        if let Self::Smoltcp(sh) = *self {
            return Some(sh);
        }
        None
    }

    pub fn kernel_handle(&self) -> Option<KernelHandle> {
        if let Self::Kernel(kh) = *self {
            return Some(kh);
        }
        None
    }
}
