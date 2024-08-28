use system_error::SystemError;
use core::intrinsics::unlikely;
use bitmap::{traits::BitMapOps, AllocBitmap};

use crate::libs::lazy_init::Lazy;
pub const PROTO_INUSE_NR: usize = 64;
// pub static mut PROTO_INUSE_IDX: Lazy<AllocBitmap> =  Lazy::new();
// pub static PROTO_INUSE_IDX: Lazy<AllocBitmap> = Lazy::new(<AllocBitmap::new(PROTO_INUSE_NR));
/// 协议操作集的trait
pub trait Protocol{
    fn close(&self);
    // fn first_false_index(&self, proto_inuse_idx:usize, proto_inuse_nr:usize)->usize;
}
/// 协议操作集的结构体
pub struct Proto<'a> {
    name: &'a str,
    // owner: THIS_MODULE,
    obj_size: usize,
    inuse_idx: Option<usize>,
}
impl Protocol for Proto<'_>{
    fn close(&self) {
    }
}
/// 静态变量，用于注册netlink协议，是一个操作集结构体的实例
// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#634
pub static mut NETLINK_PROTO: Proto = Proto {
    name: "NETLINK",
    // owner: THIS_MODULE,
    obj_size: core::mem::size_of::<Proto>(),
    // 运行时分配的索引
    inuse_idx: None,
};
// https://code.dragonos.org.cn/xref/linux-6.1.9/net/core/sock.c?fi=proto_register#3853
/// 注册协议
pub fn proto_register(proto:&mut Proto, alloc_slab:i32)->Result<i32, SystemError>{
    let mut ret = Err(SystemError::ENOBUFS);
    if alloc_slab != 0 {
        log::info!("TODO: netlink_proto: slab allocation not supported\n");
        return ret;
    }
    ret = assign_proto_idx(proto);
    ret
}
// https://code.dragonos.org.cn/xref/linux-6.1.9/net/core/sock.c?fi=proto_register#3752
/// 为协议分配一个索引
pub fn assign_proto_idx(prot: &mut Proto)->Result<i32, SystemError>{
	// prot.inuse_idx = unsafe { PROTO_INUSE_IDX.first_false_index() };
    // 如果没有找到空闲的索引
    if unlikely(prot.inuse_idx == Some(PROTO_INUSE_NR - 1)) {
		log::info!("PROTO_INUSE_NR exhausted\n");
		return Err(SystemError::ENOSPC);
	}
    // 为协议分配一个索引
	// unsafe { PROTO_INUSE_IDX.set((prot.inuse_idx).unwrap(), true) };
	return Ok(0);
}
