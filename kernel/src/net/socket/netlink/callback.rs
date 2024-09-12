use super::skbuff::SkBuff;
use crate::libs::rwlock::RwLock;
use alloc::sync::Arc;
use core::fmt::Debug;
pub trait NetlinkCallback: Send + Sync + Debug {
    /// 接收到netlink数据包时的回调函数
    fn netlink_rcv(&self, skb: Arc<RwLock<SkBuff>>) -> i32;
}
struct NetlinkCallbackData {}
