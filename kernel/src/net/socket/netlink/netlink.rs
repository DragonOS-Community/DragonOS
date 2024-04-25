//定义Netlink消息的结构体，如NLmsghdr和geNLmsghdr(拓展的netlink消息头)，以及用于封包和解包消息的函数。
//参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/netlink.h
// SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note
// Ensure the header is only included once
use core::mem;
use crate::libs::mutex::Mutex;
    pub const NETLINK_ROUTE: u32 = 0;
    pub const NETLINK_UNUSED: u32 = 1;
    pub const NETLINK_USERSOCK: u32 = 2;
    pub const NETLINK_FIREWALL: u32 = 3;
    pub const NETLINK_SOCK_DIAG: u32 = 4;
    pub const NETLINK_NFLOG: u32 = 5;
    pub const NETLINK_XFRM: u32 = 6;
    pub const NETLINK_SELINUX: u32 = 7;
    pub const NETLINK_ISCSI: u32 = 8;
    pub const NETLINK_AUDIT: u32 = 9;
    pub const NETLINK_FIB_LOOKUP: u32 = 10;
    pub const NETLINK_CONNECTOR: u32 = 11;
    pub const NETLINK_NETFILTER: u32 = 12;
    pub const NETLINK_IP6_FW: u32 = 13;
    pub const NETLINK_DNRTMSG: u32 = 14;
    //implemente uevent needed
    pub const NETLINK_KOBJECT_UEVENT: u32 = 15;
    pub const NETLINK_GENERIC: u32 = 16;
    // pub const NETLINK_DM: u32 = 17; // Assuming DM Events is unused, not defined
    pub const NETLINK_SCSITRANSPORT: u32 = 18;
    pub const NETLINK_ECRYPTFS: u32 = 19;
    pub const NETLINK_RDMA: u32 = 20;
    pub const NETLINK_CRYPTO: u32 = 21;
    pub const NETLINK_SMC: u32 = 22;

    pub const NETLINK_INET_DIAG: u32 = NETLINK_SOCK_DIAG;

    pub const MAX_LINKS: usize = 32;



    //netlink消息报头
    /**
     * struct NLmsghdr - fixed format metadata header of Netlink messages
     * @nlmsg_len:   Length of message including header
     * @nlmsg_type:  Message content type
     * @nlmsg_flags: Additional flags
     * @nlmsg_seq:   Sequence number
     * @nlmsg_pid:   Sending process port ID
     */
    #[repr(C)]
    pub struct NLmsghdr {
        pub nlmsg_len: usize,
        pub nlmsg_type: u16,
        pub nlmsg_flags: u16,
        pub nlmsg_seq: u32,
        pub nlmsg_pid: u32,
    }
    /* Flags values */


    //四种通用的消息类型 nlmsg_type
    pub const NLMSG_NOOP: u8 = 0x1; /* Nothing.     */
    pub const NLMSG_ERROR: u8 = 0x2; /* Error       */
    pub const NLMSG_DONE: u8 = 0x3; /* End of a dump    */
    pub const NLMSG_OVERRUN: u8 = 0x4; /* Data lost     */

    //消息标记 nlmsg_flags
    // pub const NLM_F_REQUEST: u32 = 1; /* It is request message.     */
    // pub const NLM_F_MULTI: u32 = 2; /* Multipart message, terminated by NLMSG_DONE */
    // pub const NLM_F_ACK: u32 = 4; /* Reply with ack, with zero or error code */
    // pub const NLM_F_ECHO: u32 = 8; /* Echo this request         */
    // pub const NLM_F_DUMP_INTR: u32 = 16; /* Dump was inconsistent due to sequence change */
    pub const NLM_F_REQUEST: u16 = 0x01;
    pub const NLM_F_MULTI: u16 = 0x02;
    pub const NLM_F_ACK: u16 = 0x04;
    pub const NLM_F_ECHO: u16 = 0x08;
    pub const NLM_F_DUMP_INTR: u16 = 0x10;
    pub const NLM_F_DUMP_FILTERED: u16 = 0x20;

    /* Modifiers to GET request */
    pub const NLM_F_ROOT: u32 = 0x100; /* specify tree root    */
    pub const NLM_F_MATCH: u32 = 0x200; /* return all matching    */
    pub const NLM_F_ATOMIC: u32 = 0x400; /* atomic GET        */
    pub const NLM_F_DUMP: u32 = NLM_F_ROOT | NLM_F_MATCH;

    /* Modifiers to NEW request */
    pub const NLM_F_REPLACE: u32 = 0x100; /* Override existing        */
    pub const NLM_F_EXCL: u32 = 0x200; /* Do not touch, if it exists    */
    pub const NLM_F_CREATE: u32 = 0x400; /* Create, if it does not exist    */
    pub const NLM_F_APPEND: u32 = 0x800; /* Add to end of list        */

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
        let new_len = len - nlmsg_align(nlmsg_len) as usize;
        nlh.add(nlmsg_align(nlmsg_len) as usize)
    }

    fn nlmsg_ok(nlh: &NLmsghdr, len: usize) -> bool {
        len >= nlmsg_hdrlen() &&
        nlh.nlmsg_len >= nlmsg_hdrlen()&&
        nlh.nlmsg_len <= len
    }

    fn nlmsg_payload(nlh: &NLmsghdr, len: usize) -> usize {
        (nlh.nlmsg_len - nlmsg_space(len)) as usize
    }

    //struct netlink_kernel_cfg,该结构包含了内核netlink的可选参数:
    struct NetlinkKernelCfg {
        groups: u32,
        flags: u32,
        //todo
        cb_mutex: *mut Mutex<()>,
    }
    impl NetlinkKernelCfg{
        fn input(){

        }

        fn bind(){

        }

        fn unbind(){

        }

        fn compare(){

        }



}
    //https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/netlink.h#229
    //netlink属性头
    pub struct NLattr {
        pub nla_len: u16,
        pub nla_type: u16,
    }