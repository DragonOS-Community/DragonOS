use alloc::{boxed::Box, sync::Arc};
use crate::libs::{mutex::Mutex, rwlock::RwLock};
use super::af_netlink::{NetlinkSock, NetlinkSocket};
// 曾用方案：在 smoltcp::PacketBuffer 的基础上封装了一层，用于处理 netlink 协议中网络数据包(skb)的相关操作
#[derive(Debug)]
#[derive(Clone)]
pub struct SkBuff {
    pub sk: Arc<Mutex<Box<dyn NetlinkSocket>>>,
    pub len: u32,
    pub pkt_type: u32,
    pub mark: u32,
    pub queue_mapping: u32,
    pub protocol: u32,
    pub vlan_present: u32,
    pub vlan_tci: u32,
    pub vlan_proto: u32,
    pub priority: u32,
    pub ingress_ifindex: u32,
    pub ifindex: u32,
    pub tc_index: u32,
    pub cb: [u32; 5],
    pub hash: u32,
    pub tc_classid: u32,
    pub data: u32,
    pub data_end: u32,
    pub napi_id: u32,
    pub family: u32,
    pub remote_ip4: u32,
    pub local_ip4: u32,
    pub remote_ip6: [u32; 4],
    pub local_ip6: [u32; 4],
    pub remote_port: u32,
    pub local_port: u32,
    pub data_meta: u32,
    pub tstamp: u64,
    pub wire_len: u32,
    pub gso_segs: u32,
    pub gso_size: u32,
    pub tstamp_type: u8,
    pub _bitfield_align_1: [u8; 0],
    pub hwtstamp: u64,
}
impl SkBuff {
    pub fn new() -> Self {
        SkBuff {
            sk: Arc::new(Mutex::new(Box::new(NetlinkSock::new()))),
            len: 0,
            pkt_type: 0,
            mark: 0,
            queue_mapping: 0,
            protocol: 0,
            vlan_present: 0,
            vlan_tci: 0,
            vlan_proto: 0,
            priority: 0,
            ingress_ifindex: 0,
            ifindex: 0,
            tc_index: 0,
            cb: [0; 5],
            hash: 0,
            tc_classid: 0,
            data: 0,
            data_end: 0,
            napi_id: 0,
            family: 0,
            remote_ip4: 0,
            local_ip4: 0,
            remote_ip6: [0; 4],
            local_ip6: [0; 4],
            remote_port: 0,
            local_port: 0,
            data_meta: 0,
            tstamp: 0,
            wire_len: 0,
            gso_segs: 0,
            gso_size: 0,
            tstamp_type: 0,
            _bitfield_align_1: [0; 0],
            hwtstamp: 0,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

// 处理网络套接字的过度运行情况
pub fn netlink_overrun(sk: &Arc<Mutex<Box<dyn NetlinkSocket>>>) {
    // Implementation of the function
}

// 用于检查网络数据包(skb)是否被共享
pub fn skb_shared(skb: &RwLock<SkBuff>) -> bool {
    // Implementation of the function
    false
}

/// 处理被孤儿化的网络数据包(skb)
/// 孤儿化网络数据包意味着数据包不再与任何套接字关联，
/// 通常是因为发送数据包时指定了 MSG_DONTWAIT 标志，这告诉内核不要等待必要的资源（如内存），而是尽可能快地发送数据包。
pub fn skb_orphan(skb: &Arc<RwLock<SkBuff>>) {
    // TODO: Implementation of the function
}

fn skb_recv_datagram(){

}

fn skb_try_recv_datagram(){
}

fn skb_try_recv_from_queue(){

}