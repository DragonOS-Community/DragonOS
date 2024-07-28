use alloc::sync::Arc;
use crate::libs::rwlock::RwLock;
use core::fmt::Debug;
use super::skbuff::SkBuff;
pub trait NetlinkCallback: Send + Sync + Debug {
    /// 接收到netlink数据包时的回调函数
    fn netlink_rcv(&self, skb: Arc<RwLock<SkBuff>>) -> i32;
}
struct NetlinkCallbackData {
    
}