use alloc::{boxed::Box, slice, sync::{Arc, Weak}, vec::Vec};
use system_error::SystemError;

//定义Netlink消息的结构体，如NLmsghdr和geNLmsghdr(拓展的netlink消息头)，以及用于封包和解包消息的函数。
//参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/netlink.h
// SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note
// Ensure the header is only included once
use crate::libs::mutex::Mutex;
use core::mem;

use super::af_netlink::{netlink_insert, Listeners, NetlinkFlags, NetlinkSock, NetlinkSocket, NL_TABLE};
// Netlink protocol family
pub const NETLINK_ROUTE: usize = 0;
pub const NETLINK_UNUSED: usize = 1;
pub const NETLINK_USERSOCK: usize = 2;
pub const NETLINK_FIREWALL: usize = 3;
pub const NETLINK_SOCK_DIAG: usize = 4;
pub const NETLINK_NFLOG: usize = 5;
pub const NETLINK_XFRM: usize = 6;
pub const NETLINK_SELINUX: usize = 7;
pub const NETLINK_ISCSI: usize = 8;
pub const NETLINK_AUDIT: usize = 9;
pub const NETLINK_FIB_LOOKUP: usize = 10;
pub const NETLINK_CONNECTOR: usize = 11;
pub const NETLINK_NETFILTER: usize = 12;
pub const NETLINK_IP6_FW: usize = 13;
pub const NETLINK_DNRTMSG: usize = 14;
// implemente uevent needed
pub const NETLINK_KOBJECT_UEVENT: usize = 15;
pub const NETLINK_GENERIC: usize = 16;
// pub const NETLINK_DM : usize = 17; // Assuming DM Events is unused, not defined
pub const NETLINK_SCSITRANSPORT: usize = 18;
pub const NETLINK_ECRYPTFS: usize = 19;
pub const NETLINK_RDMA: usize = 20;
pub const NETLINK_CRYPTO: usize = 21;
pub const NETLINK_SMC: usize = 22;

//pub const NETLINK_INET_DIAG = NETLINK_SOCK_DIAG;
pub const NETLINK_INET_DIAG: usize = 4;

pub const MAX_LINKS: usize = 32;

pub const NL_CFG_F_NONROOT_RECV: u32 = 1 << 0;
pub const NL_CFG_F_NONROOT_SEND: u32 = 1 << 1;

bitflags! {
/// 四种通用的消息类型 nlmsg_type
pub struct NLmsgType: u8 {
    /* Nothing.     */
    const NLMSG_NOOP = 0x1;
    /* Error       */
    const NLMSG_ERROR = 0x2;
    /* End of a dump    */
    const NLMSG_DONE = 0x3;
    /* Data lost     */
    const NLMSG_OVERRUN = 0x4;
}

//消息标记 nlmsg_flags
//  const NLM_F_REQUEST = 1; /* It is request message.     */
//  const NLM_F_MULTI = 2; /* Multipart message, terminated by NLMSG_DONE */
//  const NLM_F_ACK = 4; /* Reply with ack, with zero or error code */
//  const NLM_F_ECHO = 8; /* Echo this request         */
//  const NLM_F_DUMP_INTR = 16; /* Dump was inconsistent due to sequence change */
pub struct NLmsgFlags: u16 {
    /* Flags values */
    const NLM_F_REQUEST = 0x01;
    const NLM_F_MULTI = 0x02;
    const NLM_F_ACK = 0x04;
    const NLM_F_ECHO = 0x08;
    const NLM_F_DUMP_INTR = 0x10;
    const NLM_F_DUMP_FILTERED = 0x20;

    /* Modifiers to GET request */
    const NLM_F_ROOT = 0x100; /* specify tree root    */
    const NLM_F_MATCH = 0x200; /* return all matching    */
    const NLM_F_ATOMIC = 0x400; /* atomic GET        */
    //const NLM_F_DUMP = NLM_F_ROOT | NLM_F_MATCH;
    const NLM_F_DUMP = 0x100 | 0x200;

    /* Modifiers to NEW request */
    const NLM_F_REPLACE = 0x100; /* Override existing        */
    const NLM_F_EXCL = 0x200; /* Do not touch, if it exists    */
    const NLM_F_CREATE = 0x400; /* Create, if it does not exist    */
    const NLM_F_APPEND = 0x800; /* Add to end of list        */

    /* Modifiers to DELETE request */
    const NLM_F_NONREC = 0x100;	/* Do not delete recursively	*/

     /* Flags for ACK message */
    const NLM_F_CAPPED = 0x100;	/* request was capped */
    const NLM_F_ACK_TLVS = 0x200;	/* extended ACK TVLs were included */
}
}
/// netlink消息报头
/**
 * struct NLmsghdr - fixed format metadata header of Netlink messages
 * @nlmsg_len:   Length of message including header
 * @nlmsg_type:  Message content type
 * @nlmsg_flags: Additional flags
 * @nlmsg_seq:   Sequence number
 * @nlmsg_pid:   Sending process port ID
 */
pub struct NLmsghdr {
    pub nlmsg_len: usize,
    pub nlmsg_type: NLmsgType,
    pub nlmsg_flags: NLmsgFlags,
    pub nlmsg_seq: u32,
    pub nlmsg_pid: u32,
}

const NLMSG_ALIGNTO: usize = 4;
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum NetlinkState {
    NetlinkUnconnected = 0,
    NetlinkConnected,
    NETLINK_S_CONGESTED = 2,
}

fn nlmsg_align(len: usize) -> usize {
    (len + NLMSG_ALIGNTO - 1) & !(NLMSG_ALIGNTO - 1)
}

fn nlmsg_hdrlen() -> usize {
    nlmsg_align(mem::size_of::<NLmsghdr>())
}

fn nlmsg_length(len: usize) -> usize {
    len + nlmsg_hdrlen()
}

fn nlmsg_space(len: usize) -> usize {
    nlmsg_align(nlmsg_length(len))
}

unsafe fn nlmsg_data(nlh: &NLmsghdr) -> *mut u8 {
    ((nlh as *const NLmsghdr) as *mut u8).add(nlmsg_length(0))
}

unsafe fn nlmsg_next(nlh: *mut NLmsghdr, len: usize) -> *mut NLmsghdr {
    let nlmsg_len = (*nlh).nlmsg_len;
    let new_len = len - nlmsg_align(nlmsg_len);
    nlh.add(nlmsg_align(nlmsg_len))
}

fn nlmsg_ok(nlh: &NLmsghdr, len: usize) -> bool {
    len >= nlmsg_hdrlen() && nlh.nlmsg_len >= nlmsg_hdrlen() && nlh.nlmsg_len <= len
}

fn nlmsg_payload(nlh: &NLmsghdr, len: usize) -> usize {
    nlh.nlmsg_len - nlmsg_space(len)
}
// 定义类型别名来简化闭包类型的定义
type InputCallback = Arc<dyn FnMut() + Send + Sync>;
type BindCallback = Arc<dyn Fn(i32) -> i32 + Send + Sync>;
type UnbindCallback = Arc<dyn Fn(i32) -> i32 + Send + Sync>;
type CompareCallback = Arc<dyn Fn(&NetlinkSock) -> bool + Send + Sync>;
/// 该结构包含了内核netlink的可选参数:
#[derive(Default)]
pub struct NetlinkKernelCfg {
    pub groups: u32,
    pub flags: u32,
    pub input: Option<InputCallback>,
    pub bind: Option<BindCallback>,
    pub unbind: Option<UnbindCallback>,
    pub compare: Option<CompareCallback>,
}

impl NetlinkKernelCfg {
    pub fn new() -> Self {
        NetlinkKernelCfg {
            groups: 32,
            flags: 0,
            input: None,
            bind: None,
            unbind: None,
            compare: None,
        }
    }

    pub fn set_input<F>(&mut self, callback: F)
    where
        F: FnMut() + Send + Sync + 'static,
    {
        self.input = Some(Arc::new(callback));
    }

    pub fn set_bind<F>(&mut self, callback: F)
    where
        F: Fn(i32) -> i32 + Send + Sync + 'static,
    {
        self.bind = Some(Arc::new(callback));
    }

    pub fn set_unbind<F>(&mut self, callback: F)
    where
        F: Fn(i32) -> i32 + Send + Sync + 'static,
    {
        self.unbind = Some(Arc::new(callback));
    }

    pub fn set_compare<F>(&mut self, callback: F)
    where
        F: Fn(&NetlinkSock) -> bool + Send + Sync + 'static,
    {
        self.compare = Some(Arc::new(callback));
    }
}
//https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/netlink.h#229
//netlink属性头
struct NLattr {
    nla_len: u16,
    nla_type: u16,
}

pub trait VecExt {
    fn align4(&mut self);
    fn push_ext<T: Sized>(&mut self, data: T);
    fn set_ext<T: Sized>(&mut self, offset: usize, data: T);
}

impl VecExt for Vec<u8> {
    fn align4(&mut self) {
        let len = (self.len() + 3) & !3;
        if len > self.len() {
            self.resize(len, 0);
        }
    }

    fn push_ext<T: Sized>(&mut self, data: T) {
        #[allow(unsafe_code)]
        let bytes =
            unsafe { slice::from_raw_parts(&data as *const T as *const u8, size_of::<T>()) };
        for byte in bytes {
            self.push(*byte);
        }
    }

    fn set_ext<T: Sized>(&mut self, offset: usize, data: T) {
        if self.len() < offset + size_of::<T>() {
            self.resize(offset + size_of::<T>(), 0);
        }
        #[allow(unsafe_code)]
        let bytes =
            unsafe { slice::from_raw_parts(&data as *const T as *const u8, size_of::<T>()) };
        self[offset..(bytes.len() + offset)].copy_from_slice(bytes);
    }
}

// todo： net namespace
pub fn netlink_kernel_create(unit: usize, cfg:Option<NetlinkKernelCfg>) -> Result<NetlinkSock, SystemError> {
    // THIS_MODULE
	let mut nlk: NetlinkSock = NetlinkSock::new();
    let sk:Arc<Mutex<Box<dyn NetlinkSocket>>> = Arc::new(Mutex::new(Box::new(nlk.clone())));
    let groups:u32;
    if unit >= MAX_LINKS {
        return Err(SystemError::EINVAL);
    }
    __netlink_create(&mut nlk, unit, 1).expect("__netlink_create failed");

    if let Some(cfg) = cfg.as_ref() {
        if cfg.groups < 32 {
            groups = 32;
        } else {
            groups = cfg.groups;
        }
    } else {
        groups = 32;
    }
    let listeners = Listeners::new();
    // todo：设计和实现回调函数
    // sk.sk_data_read = netlink_data_ready;
    // if cfg.is_some() && cfg.unwrap().input.is_some(){
    //     nlk.netlink_rcv = cfg.unwrap().input;
    // }
    netlink_insert(sk,0).expect("netlink_insert failed");
    nlk.flags |= NetlinkFlags::NETLINK_F_KERNEL_SOCKET.bits();

    let mut nl_table = NL_TABLE.write();
    if nl_table[unit].get_registered()==0 {
            nl_table[unit].set_groups(groups);
            if let Some(cfg) = cfg.as_ref() {
                nl_table[unit].bind = cfg.bind.clone();
                nl_table[unit].unbind = cfg.unbind.clone();
                nl_table[unit].set_flags(cfg.flags);
                if cfg.compare.is_some() {
                    nl_table[unit].compare = cfg.compare.clone();
            }
            nl_table[unit].set_registered(1);
        } else {
            drop(listeners);
            let registered = nl_table[unit].get_registered();
            nl_table[unit].set_registered(registered + 1);
        }
    }
    return Ok(nlk);
}

fn __netlink_create(nlk: &mut NetlinkSock, unit: usize, kern:usize)->Result<i32,SystemError>{
    // 其他的初始化配置参数
    nlk.flags = kern as u32;
    nlk.protocol = unit;
    return Ok(0);
}

pub fn sk_data_ready(nlk: Arc<NetlinkSock>)-> Result<(),SystemError>{
    // 唤醒
    return Ok(());
}