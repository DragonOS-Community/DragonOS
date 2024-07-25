use core::cell::RefCell;


use alloc::{rc::Rc, sync::Arc};
use smoltcp::socket::udp::{PacketBuffer, PacketMetadata};

use crate::libs::rwlock::RwLock;

use super::af_netlink::{NetlinkSock, NetlinkSocket};
// 在 smoltcp::PacketBuffer 的基础上封装了一层，用于处理 netlink 协议中网络数据包(skb)的相关操作
#[derive(Debug)]
pub struct SkBuff<'a> {
    inner: Arc<PacketBuffer<'a>>,
}

impl<'a> SkBuff<'a> {
    pub fn new() -> Self {
        Self {
            // TODO: 666 is a magic number, should be replaced by a constant
            inner: Arc::new(PacketBuffer::new(vec![PacketMetadata::EMPTY; 666], vec![0; 666])),
        }
    }
    pub fn inner(&self) -> &PacketBuffer<'a> {
        &self.inner
    }
    pub fn inner_mut(&mut self) -> &mut PacketBuffer<'a> {
        &mut self.inner
    }
    pub fn clone_with_new_inner(&self) -> Self {
        // TODO: 666 is a magic number, should be replaced by a constant
        Self {
            inner: Arc::new(PacketBuffer::new(vec![PacketMetadata::EMPTY; 666], vec![0; 666])),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
    pub fn clone(&self) -> Self {
        SkBuff {
            inner: Arc::clone(&self.inner),
        }
    }
}

// 处理网络套接字的过度运行情况
pub fn netlink_overrun(sk: &Arc<dyn NetlinkSocket>) {
    // Implementation of the function
}

// 用于检查网络数据包(skb)是否被共享
pub fn skb_shared(skb: &RwLock<SkBuff>) -> bool {
    // Implementation of the function
    false
}

// 处理被孤儿化的网络数据包(skb)。
// 孤儿化网络数据包意味着数据包不再与任何套接字关联，
// 通常是因为发送数据包时指定了MSG_DONTWAIT标志，这告诉内核不要等待必要的资源（如内存），而是尽可能快地发送数据包。
pub fn skb_orphan(skb: &RwLock<SkBuff>) {
    // Implementation of the function
}

// 网络数据包(skb)的克隆操作
pub fn skb_clone(skb: Rc<RefCell<SkBuff>>, allocation: u32) -> Rc<RefCell<SkBuff>> {
    // Implementation of the function
    Rc::new(RefCell::new(SkBuff::new()))
}

// 增加网络数据包(skb)的使用者计数
pub fn skb_get(skb: Rc<RefCell<SkBuff>>) -> Rc<RefCell<SkBuff>> {
    // Implementation of the function
    Rc::new(RefCell::new(SkBuff::new()))
}

// 用于释放网络套接字(sk)的资源。
pub fn sock_put(sk: &Arc<dyn NetlinkSocket>) {
    // Implementation of the function
}
