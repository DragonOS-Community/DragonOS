//定义Netlink消息的结构体，如NLmsghdr和geNLmsghdr(拓展的netlink消息头)，以及用于封包和解包消息的函数。
//参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/netlink.h
// SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note
// Ensure the header is only included once
use crate::libs::mutex::Mutex;
use core::mem;
bitflags! {
pub struct NETLINK_PROTO :u32 {
    const NETLINK_ROUTE = 0;
    const NETLINK_UNUSED = 1;
    const NETLINK_USERSOCK = 2;
    const NETLINK_FIREWALL = 3;
    const NETLINK_SOCK_DIAG = 4;
    const NETLINK_NFLOG = 5;
    const NETLINK_XFRM = 6;
    const NETLINK_SELINUX = 7;
    const NETLINK_ISCSI = 8;
    const NETLINK_AUDIT = 9;
    const NETLINK_FIB_LOOKUP = 10;
    const NETLINK_CONNECTOR = 11;
    const NETLINK_NETFILTER = 12;
    const NETLINK_IP6_FW = 13;
    const NETLINK_DNRTMSG = 14;
    // implemente uevent needed
    const NETLINK_KOBJECT_UEVENT = 15;
    const NETLINK_GENERIC = 16;
    // const NETLINK_DM = 17; // Assuming DM Events is unused, not defined
    const NETLINK_SCSITRANSPORT = 18;
    const NETLINK_ECRYPTFS = 19;
    const NETLINK_RDMA = 20;
    const NETLINK_CRYPTO = 21;
    const NETLINK_SMC = 22;

    //const NETLINK_INET_DIAG = NETLINK_SOCK_DIAG;
    const NETLINK_INET_DIAG = 4;

    const MAX_LINKS = 32;
}



//netlink消息报头
/**
 * struct NLmsghdr - fixed format metadata header of Netlink messages
 * @nlmsg_len:   Length of message including header
 * @nlmsg_type:  Message content type
 * @nlmsg_flags: Additional flags
 * @nlmsg_seq:   Sequence number
 * @nlmsg_pid:   Sending process port ID
 */

//四种通用的消息类型 nlmsg_type
pub struct NLmsgType: u8 {
    const NLMSG_NOOP = 0x1; /* Nothing.     */
    const NLMSG_ERROR = 0x2; /* Error       */
    const NLMSG_DONE = 0x3; /* End of a dump    */
    const NLMSG_OVERRUN = 0x4; /* Data lost     */
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

struct NLmsghdr {
    nlmsg_len: usize,
    nlmsg_type: u16,
    nlmsg_flags: u16,
    nlmsg_seq: u32,
    nlmsg_pid: u32,
}

const NLMSG_ALIGNTO: usize = 4;

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

//struct netlink_kernel_cfg,该结构包含了内核netlink的可选参数:
struct NetlinkKernelCfg {
    groups: usize,
    flags: usize,
    //todo about mutex
    cb_mutex: *mut Mutex<()>,
}
impl NetlinkKernelCfg {
    fn input() {}

    fn bind() {}

    fn unbind() {}

    fn compare() {}
}
//https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/netlink.h#229
//netlink属性头
struct NLattr {
    nla_len: u16,
    nla_type: u16,
}
