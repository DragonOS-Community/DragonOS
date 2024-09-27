use alloc::sync::Arc;
use netlink::NETLINK_KOBJECT_UEVENT;
use system_error::SystemError;

use crate::driver::base::uevent::KobjUeventEnv;

use super::{family, inet::datagram, Inode, Socket, Type};

//https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/
/*
..		-	-
Kconfig
Makefile
af_netlink.c
af_netlink.h
diag.c  Netlink 套接字的诊断功能，主要用于查询内核中存在的 Netlink 套接字信息
genetlink.c
policy.c
*/
// Top-level module defining the public API for Netlink
pub mod af_netlink;
pub mod callback;
pub mod endpoint;
pub mod netlink;
pub mod netlink_proto;
pub mod skbuff;
pub mod sock;

pub struct Netlink;

impl family::Family for Netlink {
    /// 用户空间创建一个新的套接字的入口
    fn socket(stype: Type, _protocol: u32) -> Result<Arc<Inode>, SystemError> {
        let socket = create_netlink_socket(_protocol)?;
        Ok(Inode::new(socket))
    }
}
/// 用户空间创建一个新的Netlink套接字
fn create_netlink_socket(
    _protocol: u32,
) -> Result<Arc<dyn Socket>, SystemError> {
    match _protocol as usize {
        NETLINK_KOBJECT_UEVENT => {
            Ok(Arc::new(af_netlink::NetlinkSock::new()))
        }
        _ => {
            Err(SystemError::EPROTONOSUPPORT)
        }
    }
}