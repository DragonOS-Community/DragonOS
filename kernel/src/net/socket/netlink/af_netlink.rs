//参考https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c

// netlink_proto结构体
// static mut NETLINK_PROTO: netlink_proto::NetlinkProto = netlink_proto::NetlinkProto {
//     name: String::from("NETLINK"),
//     owner: std::sync::Arc::downgrade(THIS_MODULE),
//     obj_size: std::mem::size_of::<netlink_sock>(),
// };

// SPDX-License-Identifier: GPL-2.0

use core::{any::Any, cell::RefCell, fmt::Debug, hash::Hash, ops::Deref};

use alloc::{
    string::String,
    sync::{Arc, Weak},
};
use driver_base_macros::get_weak_or_clear;
use intertrait::CastFromSync;
use system_error::SystemError;

use crate::{
    filesystem::{
        kernfs::KernFSInode,
        sysfs::{sysfs_instance, Attribute, AttributeGroup, SysFSOps, SysFSOpsSupport},
    }, include::bindings::bindings::kzalloc, libs::{
        casting::DowncastArc, mutex::Mutex, rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard}
    }, net::socket::SocketType
};use alloc::{boxed::Box, vec::Vec};

use crate::net::socket::Socket;
use lazy_static::lazy_static;
use smoltcp::socket::raw::PacketBuffer;
use smoltcp::socket::raw::PacketMetadata;
// Flags constants
bitflags! {
    pub struct NetlinkFlags: u32 {
        const KERNEL_SOCKET = 0x1;
        const RECV_PKTINFO = 0x2;
        const BROADCAST_SEND_ERROR = 0x4;
        const RECV_NO_ENOBUFS = 0x8;
        const LISTEN_ALL_NSID = 0x10;
        const CAP_ACK = 0x20;
        const EXT_ACK = 0x40;
        const STRICT_CHK = 0x80;
        const NETLINK_F_KERNEL_SOCKET = 0x100;
    }
}

pub struct sockaddr_nl {
    // pub nl_family: SA_FAMILY_T,
    pub nl_pad: u16,
    pub nl_pid: u32,
    pub nl_groups: u32,
}

trait NetlinkSocket {
    fn send(&self, msg: &[u8]) -> Result<(),SystemError>;
    fn recv(&self) -> Result<Vec<u8>,SystemError>;
    fn bind(&self) -> Result<(),SystemError>;
    fn unbind(&self) -> Result<(),SystemError>;
    fn register(&self, listener: Box<dyn NetlinkMessageHandler>);
    fn unregister(&self, listener: Box<dyn NetlinkMessageHandler>);

}

pub struct HListHead<T> {
    first: Option<Box<HListNode<T>>>,
}

pub struct HListNode<T> {
    data: T,
    next: Option<Box<HListNode<T>>>,
}

pub struct NetlinkTable {
    listeners: Option<listeners>,
    registed: u32,
    flags: u32,
    groups: u32,
    mc_list:HListHead<NetlinkSock>,
}
impl NetlinkTable{
    fn new() -> NetlinkTable {
        NetlinkTable {
            listeners: Some(listeners {
                masks: Vec::new(),
            }),
            registed: 0,
            flags: 0,
            groups: 0,
            mc_list:HListHead {
                first: None,
            },
        }
    }
    // fn hash(&self)->RhashTable;
    fn listeners(&self)->RCuListeners{
        RCuListeners::new()
    }
    fn flags(&self)->u32{
        0
    }
    fn groups(&self)->u32{
        0
    }
    // fn cb_mutex(&self)->Mutex;
    // fn module(&self)->Module;
    // fn bind(net:Net, group: u32);
    // fn unbind(net:Net, group: u32);
    // fn compare(net:Net, sock:sock);
    fn registed(&self)->u32{
        0
    }
    // fn bind(&self, net: &Net, group: u32) {
    //     // Implementation of bind
    // }
    // fn unbind(&self, net: &Net, group: u32) {
    //     // Implementation of unbind
    // }
    // fn compare(&self, net: &Net, sock: &sock) {
    //     // Implementation of compare
    // }
}

pub struct LockedNetlinkTable(RwLock<NetlinkTable>);

impl LockedNetlinkTable {
    pub fn new(netlinktable: NetlinkTable) ->LockedNetlinkTable  {
        LockedNetlinkTable(RwLock::new(netlinktable))
    }
}
// You would need to implement the actual methods for the traits and the bind/unbind functions.
trait NetlinkMessageHandler {
    fn handle_message(&mut self, msg: &[u8]) {
        // Implementation of message handling
    }
}

struct RCuListeners {
    list: Vec<Box<dyn NetlinkMessageHandler>>,
}

impl RCuListeners {
    fn new() -> Self {
        Self { list: Vec::new() }
    }

    fn register(&mut self, listener: Box<dyn NetlinkMessageHandler>) {
        self.list.push(listener);
    }

    fn handle_message(&mut self, msg: &[u8]) {
        for listener in &mut self.list {
            listener.handle_message(msg);
        }
    }
}

// You would need to implement the actual methods for the NetlinkTable and NetlinkSocket structs.
// netlink初始化函数
//https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#2916

const MAX_LINKS: usize = 32;

fn netlink_proto_init() -> Result<(),SystemError> {

    // 条件编译，类似于C语言中的#if defined(...)
    // #[cfg(all(feature = "CONFIG_BPF_SYSCALL", feature = "CONFIG_PROC_FS"))]
    // {
    //     // 注册BPF迭代器
    //     // ...
    // }
    
    
    // 创建NetlinkTable,每种netlink协议类型占数组中的一项，后续内核中创建的不同种协议类型的netlink都将保存在这个表中，由该表统一维护
    // 检查NetlinkTable的大小是否符合预期
    let mut nl_table = [0; MAX_LINKS];
    if nl_table.is_empty() {
        panic!("netlink_init: Cannot allocate nl_table");
    }
    // 初始化哈希表
    // for i in 0..MAX_LINKS {
    //     if rhashtable_init(&mut nl_table[i], &netlink_rhashtable_params) < 0 {
    //         while i > 0 {
    //             i -= 1;
    //             rhashtable_destroy(&mut nl_table[i].hash);
    //         }
    //         drop(nl_table); // This replaces kfree in Rust
    //         panic!("netlink_init: Cannot allocate nl_table");
    //     }
    // }
    
    //netlink_add_usersock_entry();
	//sock_register(&netlink_family_ops);
	//register_pernet_subsys(&netlink_net_ops);
	//register_pernet_subsys(&netlink_tap_net_ops);
	/* The netlink device handler may be needed early. */
	//rtnetlink_init();

    // 如果一切正常，返回Ok(())
    Ok(())
}

fn main() {
    // 调用初始化函数，并处理可能的错误
    if let Err(e) = netlink_proto_init() {
        // 如果出现错误，打印错误信息并退出
    }
}


// You will need to implement the following types and functions:
// - NetlinkProto
// - proto_register
// - bpf_iter_register
// - RhashTable
// - netlink_add_usersock_entry
// - sock_register
// - register_pernet_subsys
// - rtnetlink_init
// ...

//内核初始化函数注册
//Linux：core_initcall(netlink_proto_init);


//https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#672

enum Error {
    SocketTypeNotSupported,
    ProtocolNotSupported,
}
#[derive(PartialEq)]
pub struct sock{
    sk_protocol: usize,
    sk_family: i32,
    sk_state: i32,
    sk_flags: u32,
    sk_type: SocketType,
    // sk_prot: dyn proto,
    // sk_net: Net,
    sk_bound_dev_if: i32,
    
}
pub trait socktrait{
    fn sk(&self) -> &sock;
    // fn sk_prot(&self) -> &dyn proto;
    fn sk_family(&self) -> i32;
    fn sk_state(&self) -> i32;
    fn sk_protocol(&self) -> usize;
    fn is_kernel(&self) -> bool;
}
impl socktrait for sock{
    fn sk(&self) -> &sock{
        self
    }
    // fn sk_prot(&self) -> &dyn proto;
    fn sk_family(&self) -> i32{
        0
    }
    fn sk_state(&self) -> i32{
        0
    }
    fn sk_protocol(&self) -> usize{
        self.sk_protocol
    }
    fn is_kernel(&self) -> bool{
        true
    }
}

/* linux：struct sock has to be the first member of netlink_sock */
pub struct NetlinkSock{
    sk: sock,
    portid: u32,
    dst_portid: u32,
    dst_group: u32,
    flags: u32,
    subscriptions: u32,
    ngroups: u32,
    groups: Vec<u64>,
    state: u64,
    max_recvmsg_len: usize,
    dump_done_errno: i32,
    cb_running: bool,
}


impl socktrait for NetlinkSock {
    fn sk(&self) -> &sock{
        &self.sk
    }
    fn sk_family(&self) -> i32{
        0
    }
    fn sk_state(&self) -> i32{
        0
    }
    fn sk_protocol(&self) -> usize{
        0
    }
    fn is_kernel(&self) -> bool{
        true
    }
    
}

// https://code.dragonos.org.cn/s?refs=netlink_create&project=linux-6.1.9
fn netlink_create(socket: &mut dyn Socket, protocol: i32, _kern: bool) -> Result<(), Error> {



    // 假设我们有一个类型来跟踪协议最大值
    const MAX_LINKS: i32 = 1024;

    // if socket.type_ != SocketType::Raw && socket.type_ != SocketType::Dgram {
    //     return Err(Error::SocketTypeNotSupported);
    // }

    if protocol < 0 || protocol >= MAX_LINKS {
        return Err(Error::ProtocolNotSupported);
    }

    // 安全的数组索引封装
    let protocol = protocol as usize;

    // 这里简化了锁和模块加载逻辑

    // 假设成功加载了模块和相关函数


    // 继续其他的逻辑
    // ...

    Ok(())
}

// 假设的绑定和解绑函数
// fn bind_function(net: &Net, group: usize) {
//     // 实现细节
// }

// fn unbind_function(net: &Net, group: usize) {
//     // 实现细节
// }
struct callback_head {
    next: Option<Box<callback_head>>,
}
impl callback_head {
    fn next(&self) -> &Option<Box<callback_head>> {
        &self.next
    }
    fn func(&self) -> Option<Box<dyn Fn() -> i32>> {
        None
    }
}

struct listeners {
    // Recursive Wakeup Unlocking?
	masks:Vec<u64>,
}
impl listeners{
    fn masks(&self)->Vec<u64>{
        Vec::new()
    }
}

lazy_static! {
    static ref NL_TABLE: RwLock<Vec<NetlinkTable>> = RwLock::new(vec![NetlinkTable::new()]);
}
pub fn netlink_has_listeners(sk: &sock, group: u32) -> i32 {
    let mut res = 0;
    let nl_table = NL_TABLE.read();
    if let Some(listeners) = &nl_table[sk.sk_protocol].listeners {
        if group - 1 < nl_table[sk.sk_protocol as usize].groups {
            res = listeners.masks[group as usize - 1]as i32;
        }
    }
    res
}
pub struct SkBuff<'a> {
    inner: PacketBuffer<'a>,
}

impl<'a> SkBuff<'a> {
    pub fn new() -> Self {
        Self {
            inner: PacketBuffer::new(vec![PacketMetadata::EMPTY; 666],
                vec![0; 666],) 
        }
    }
    pub fn inner(&self) -> &PacketBuffer<'a> {
        &self.inner
    }
    pub fn inner_mut(&mut self) -> &mut PacketBuffer<'a> {
        &mut self.inner
    }
    pub fn clone_with_new_inner(&self) -> Self {
        Self {
            inner: PacketBuffer::new(vec![PacketMetadata::EMPTY; 666],
                vec![0; 666],) 
        }
    }
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}
struct NetlinkBroadcastData<'a> {
	exclude_sk:&'a sock,
    // net: &'a Net,
    portid:u32,
	group:u32,
	failure:i32,
	delivery_failure:i32,
	congested:i32,
	delivered:i32,
    allocation:u32,
	skb:Arc<RefCell<SkBuff<'a>>>,
    skb_2:Arc<RefCell<SkBuff<'a>>>,
}
impl<'a> NetlinkBroadcastData<'a> {
    pub fn copy_skb_to_skb_2(&mut self){
        let skb = self.skb.borrow().clone_with_new_inner();
        *self.skb_2.borrow_mut() = skb;
    }
}
fn sk_for_each_bound(sk: &sock, mc_list: &HListHead<NetlinkSock>) {
    let mut node = mc_list.first.as_ref();
    while let Some(n) = node {
        let data = &n.data;
        if data.sk.sk_protocol == sk.sk_protocol {
            // Implementation of the function
        }
        node = n.next.as_ref();
    }
}
/*
static void do_one_broadcast(struct sock *sk,
				    struct netlink_broadcast_data *p)
{
	struct netlink_sock *nlk = nlk_sk(sk);
	int val;

	if (p->exclude_sk == sk)
		return;

	if (nlk->portid == p->portid || p->group - 1 >= nlk->ngroups ||
	    !test_bit(p->group - 1, nlk->groups))
		return;

	if (!net_eq(sock_net(sk), p->net)) {
		if (!(nlk->flags & NETLINK_F_LISTEN_ALL_NSID))
			return;

		if (!peernet_has_id(sock_net(sk), p->net))
			return;

		if (!file_ns_capable(sk->sk_socket->file, p->net->user_ns,
				     CAP_NET_BROADCAST))
			return;
	}

	if (p->failure) {
		netlink_overrun(sk);
		return;
	}

	sock_hold(sk);
	if (p->skb2 == NULL) {
		if (skb_shared(p->skb)) {
			p->skb2 = skb_clone(p->skb, p->allocation);
		} else {
			p->skb2 = skb_get(p->skb);
			/*
			 * skb ownership may have been set when
			 * delivered to a previous socket.
			 */
			skb_orphan(p->skb2);
		}
	}
	if (p->skb2 == NULL) {
		netlink_overrun(sk);
		/* Clone failed. Notify ALL listeners. */
		p->failure = 1;
		if (nlk->flags & NETLINK_F_BROADCAST_SEND_ERROR)
			p->delivery_failure = 1;
		goto out;
	}
	if (sk_filter(sk, p->skb2)) {
		kfree_skb(p->skb2);
		p->skb2 = NULL;
		goto out;
	}
	NETLINK_CB(p->skb2).nsid = peernet2id(sock_net(sk), p->net);
	if (NETLINK_CB(p->skb2).nsid != NETNSA_NSID_NOT_ASSIGNED)
		NETLINK_CB(p->skb2).nsid_is_set = true;
	val = netlink_broadcast_deliver(sk, p->skb2);
	if (val < 0) {
		netlink_overrun(sk);
		if (nlk->flags & NETLINK_F_BROADCAST_SEND_ERROR)
			p->delivery_failure = 1;
	} else {
		p->congested |= val;
		p->delivered = 1;
		p->skb2 = NULL;
	}
out:
	sock_put(sk);
}
*/
fn do_one_broadcast(sk: &sock, info: &NetlinkBroadcastData) {
    let mut sk = sk;
    let mut info = info;
    let ret:i32;
    if info.exclude_sk==sk{
        return;
    }

}
pub fn netlink_broadcast<'a>(ssk: &'a sock, skb: Arc<RefCell<SkBuff<'a>>>, portid: u32, group: u32, allocation: u32) -> Result<(), SystemError> {
    // let net = sock_net(ssk);
    let mut info = NetlinkBroadcastData {
        exclude_sk: ssk,
        // net,
        portid,
        group,
        failure: 0,
        delivery_failure: 0,
        congested: 0,
        delivered: 0,
        allocation,
        skb,
        skb_2: Arc::new(RefCell::new(SkBuff::new())),
    };

    // While we sleep in clone, do not allow to change socket list
    let nl_table = NL_TABLE.read();
    for sk in &nl_table[ssk.sk_protocol].mc_list {
        do_one_broadcast(sk, &info);
    }
    

    // consume_skb(skb);


    if info.delivery_failure != 0 {
        drop(info.skb_2);
        return Err(SystemError::ENOBUFS);
    }
    // consume_skb(info.skb_2);

    // if info.delivered != 0 {
    //     if info.congested != 0 && gfpflags_allow_blocking(allocation) != 0 {
    //         yield_();
    //     }
    //     return Ok(());
    // }
    return Err(SystemError::ESRCH);

}