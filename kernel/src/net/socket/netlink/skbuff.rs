use super::af_netlink::{NetlinkSock, NetlinkSocket};
use crate::libs::{mutex::Mutex, rwlock::RwLock};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use uefi_raw::protocol;
const SKB_SIZE: usize = 4096; // 定义 SKB 的大小
#[derive(Debug, Clone)]
pub struct SkBuff {
    pub sk: Arc<Mutex<NetlinkSock>>,
    pub len: u32,
    pub inner: Vec<u8>,
}
impl SkBuff {
    pub fn new(protocol: Option<usize>) -> Self {
        SkBuff {
            sk: Arc::new(Mutex::new(NetlinkSock::new(protocol))),
            len: 0,
            inner: vec![0u8; SKB_SIZE],
        }
    }
}

// 处理网络套接字的过度运行情况
pub fn netlink_overrun(sk: &Arc<Mutex<NetlinkSock>>) {
    todo!()
}

// 用于检查网络数据包(skb)是否被共享
pub fn skb_shared(skb: &RwLock<SkBuff>) -> bool {
    // todo!()
    false
}

/// 处理被孤儿化的网络数据包(skb)
/// 孤儿化网络数据包意味着数据包不再与任何套接字关联，
/// 通常是因为发送数据包时指定了 MSG_DONTWAIT 标志，这告诉内核不要等待必要的资源（如内存），而是尽可能快地发送数据包。
pub fn skb_orphan(skb: &Arc<RwLock<SkBuff>>) {
    // todo!()
}

fn skb_recv_datagram() {}

fn skb_try_recv_datagram() {}

fn skb_try_recv_from_queue() {}
