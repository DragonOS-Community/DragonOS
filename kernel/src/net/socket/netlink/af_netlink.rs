//参考https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c


use core::mem::size_of;
use core::{any::Any, cell::RefCell, fmt::Debug, hash::Hash, ops::Deref};

use alloc::rc::Rc;
use alloc::string::String;
use alloc::sync::Arc;

use hashbrown::HashMap;
use intertrait::CastFromSync;
use num::Zero;
use smoltcp::wire::IpListenEndpoint;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::libs::rwlock::RwLockWriteGuard;
use crate::net::socket::netlink::skbuff::SkBuff;
use crate::{
    libs::rwlock::RwLock,
    net::{net_core::consume_skb, socket::SocketType},
    syscall::Syscall,
};
use alloc::{boxed::Box, vec::Vec};

use crate::net::socket::Socket;
use lazy_static::lazy_static;
use smoltcp::socket::raw::PacketBuffer;
use smoltcp::socket::raw::PacketMetadata;

use super::netlink::{NETLINK_USERSOCK, NL_CFG_F_NONROOT_SEND};
use super::netlink_proto::{proto_register, Proto, NETLINK_PROTO};
use super::skbuff::{netlink_overrun, skb_orphan, skb_shared, sock_hold, sock_put};

use crate::init::initcall::INITCALL_CORE;
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

pub struct SockaddrNl {
    // pub nl_family: SA_FAMILY_T,
    pub nl_pad: u16,
    pub nl_pid: u32,
    pub nl_groups: u32,
}

pub struct HListHead {
    first: Option<Arc<HListNode>>,
}

pub struct HListNode {
    data: Arc<dyn NetlinkSocket>,
    next: Option<Arc<HListNode>>,
}
impl HListHead {
    fn iter(&self) -> HListHeadIter {
        HListHeadIter {
            current: self.first.as_ref(),
        }
    }
}

struct HListHeadIter<'a> {
    current: Option<&'a Arc<HListNode>>,
}

impl<'a> Iterator for HListHeadIter<'a> {
    type Item = &'a Arc<dyn NetlinkSocket>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.current {
            Some(node) => {
                self.current = node.next.as_ref();
                Some(&node.data)
            }
            None => None,
        }
    }
}

pub struct NetlinkTable {
    hash: HashMap<u32, Arc<dyn NetlinkSocket>>,
    listeners: Option<listeners>,
    registered: u32,
    flags: u32,
    groups: i32,
    mc_list: HListHead,
}
impl NetlinkTable {
    fn new() -> NetlinkTable {
        NetlinkTable {
            hash: HashMap::new(),
            listeners: Some(listeners { masks: Vec::new() }),
            registered: 0,
            flags: 0,
            groups: 0,
            mc_list: HListHead { first: None },
        }
    }
    // fn hash(&self)->RhashTable;
    fn listeners(&self) -> RCuListeners {
        RCuListeners::new()
    }
    fn flags(&self) -> u32 {
        0
    }
    fn groups(&self) -> u32 {
        0
    }
    // fn cb_mutex(&self)->Mutex;
    // fn module(&self)->Module;
    // fn bind(net:Net, group: u32);
    // fn unbind(net:Net, group: u32);
    // fn compare(net:Net, sock:sock);
    fn registed(&self) -> u32 {
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
    pub fn new(netlinktable: NetlinkTable) -> LockedNetlinkTable {
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


 
// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#2916
/// netlink 协议的最大数量
const MAX_LINKS: usize = 32;
#[unified_init(INITCALL_CORE)]
/// netlink 协议的初始化函数
fn netlink_proto_init() -> Result<(), SystemError> {
	unsafe{ 
        let err = proto_register(&mut NETLINK_PROTO, 0);
        if err.is_err(){
            return Err(SystemError::ENOSYS)
        }
    }
    // 创建NetlinkTable,每种netlink协议类型占数组中的一项，后续内核中创建的不同种协议类型的netlink都将保存在这个表中，由该表统一维护
    // 检查NetlinkTable的大小是否符合预期
    let mut nl_table = NL_TABLE.write();
    // let mut nl_table = [0; MAX_LINKS];
    if nl_table.is_empty() {
        panic!("netlink_init: Cannot allocate nl_table");
    }
    // 初始化哈希表
    for i in 0..MAX_LINKS {
        nl_table[i].hash = HashMap::new();
    }
    // 将读写锁守卫作为参数传递，避免锁的重复获取造成阻塞
    netlink_add_usersock_entry(&mut nl_table);
    // TODO: 以下函数需要 net namespace 支持
    // sock_register(&netlink_family_ops);
    // register_pernet_subsys(&netlink_net_ops);
    // register_pernet_subsys(&netlink_tap_net_ops);
    /* The netlink device handler may be needed early. */
    // rtnetlink_init();
    Ok(())
}

/// 初始化和注册一个用户套接字条目，并将其添加到全局的NetlinkTable向量中
pub fn netlink_add_usersock_entry(nl_table: &mut RwLockWriteGuard<Vec<NetlinkTable>>)
{
	let listeners: Option<listeners> = Some(listeners::new());
	let groups: i32 = 32;
	if listeners.is_none(){
        panic!("netlink_add_usersock_entry: Cannot allocate listeners\n");
    }

    let index = NETLINK_USERSOCK;
	nl_table[index].groups = groups;
	// rcu_assign_pointer(nl_table[index].listeners, listeners);
	// nl_table[index].module = THIS_MODULE;
	nl_table[index].registered = 1;
	nl_table[index].flags = NL_CFG_F_NONROOT_SEND;
}

/// 
fn netlink_insert(){

}

/// 
fn netlink_lookup(){

}

// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#672

enum Error {
    SocketTypeNotSupported,
    ProtocolNotSupported,
}

// netlink机制特定的内核抽象，不同于标准的trait Socket
pub trait NetlinkSocket: Any + Send + Sync + Debug + CastFromSync {
    // fn sk_prot(&self) -> &dyn proto;
    fn sk_family(&self) -> i32;
    fn sk_state(&self) -> i32;
    fn sk_protocol(&self) -> usize;
    fn is_kernel(&self) -> bool;
    fn equals(&self, other: &dyn NetlinkSocket) -> bool;
    fn portid(&self) -> u32;
    fn ngroups(&self) -> u64;
    fn groups(&self) -> Vec<u64>;
    fn flags(&self) -> u32;
}

/* linux：struct sock has to be the first member of netlink_sock */
// linux 6.1.9中的netlink_sock结构体里，sock是一个很大的结构体，这里简化
// 意义是netlink_sock（NetlinkSock）是一个sock（NetlinkSocket）
#[derive(Debug)]
pub struct NetlinkSock {
    sk: Box<dyn NetlinkSocket>,
    portid: u32,
    dst_portid: u32,
    dst_group: u32,
    flags: u32,
    subscriptions: u32,
    ngroups: u64,
    groups: Vec<u64>,
    state: u64,
    max_recvmsg_len: usize,
    dump_done_errno: i32,
    cb_running: bool,
}

impl NetlinkSocket for NetlinkSock {
    fn sk_family(&self) -> i32 {
        0
    }
    fn sk_state(&self) -> i32 {
        0
    }
    fn sk_protocol(&self) -> usize {
        0
    }
    fn is_kernel(&self) -> bool {
        true
    }
    fn equals(&self, other: &dyn NetlinkSocket) -> bool {
        // compare the fields of self and other
        // use the equals method to compare the NetlinkSocket objects
        self.sk.equals(other)
    }
    fn portid(&self) -> u32 {
        0
    }
    fn ngroups(&self) -> u64 {
        0
    }
    fn groups(&self) -> Vec<u64> {
        Vec::new()
    }
    fn flags(&self) -> u32 {
        0
    }
}
impl PartialEq for NetlinkSock {
    fn eq(&self, other: &Self) -> bool {
        // compare the fields of self and other
        // use the equals method to compare the NetlinkSocket objects
        self.sk.equals(&*other.sk)
    }
}

impl NetlinkSock {
    fn send(&self, msg: &[u8]) -> Result<(), SystemError> {
        // Implementation of the function
        Ok(())
    }

    fn recv(&self) -> Result<Vec<u8>, SystemError> {
        // Implementation of the function
        Ok(Vec::new())
    }

    fn bind(&self) -> Result<(), SystemError> {
        // Implementation of the function
        Ok(())
    }

    fn unbind(&self) -> Result<(), SystemError> {
        // Implementation of the function
        Ok(())
    }

    fn register(&self, listener: Box<dyn NetlinkMessageHandler>) {
        // Implementation of the function
    }
    fn unregister(&self, listener: Box<dyn NetlinkMessageHandler>) {
        // Implementation of the function
    }
}

// impl Socket for NetlinkSock {
//     fn read(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
//         // Implementation of the function
//         Ok(0)
//     }
//     fn write(&self, buf: &[u8]) -> Result<usize, SystemError> {
//         // Implementation of the function
//         Ok(0)
//     }
//     fn close(&self) {
//         // Implementation of the function
//     }
//     fn connect(&mut self, _endpoint: crate::net::Endpoint) -> Result<(), SystemError> {
//         // Implementation of the function
//         Ok(())
//     }
//     fn bind(&mut self, _endpoint: crate::net::Endpoint) -> Result<(), SystemError> {
//         // Implementation of the function
//         Ok(())
//     }
//     fn shutdown(&mut self, _type: crate::net::ShutdownType) -> Result<(), SystemError> {
//         // Implementation of the function
//         Ok(())
//     }
//     fn listen(&mut self, _backlog: usize) -> Result<(), SystemError> {
//         // Implementation of the function
//         Ok(())
//     }
//     fn accept(&mut self) -> Result<(Box<dyn Socket>, crate::net::Endpoint), SystemError> {
//         // Implementation of the function
//         Ok((Box::new(NetlinkSock::new()), crate::net::Endpoint::new()))
//     }
//     fn endpoint(&self) -> Option<crate::net::Endpoint> {
//         // Implementation of the function
//         None
//     }
//     fn peer_endpoint(&self) -> Option<crate::net::Endpoint> {
//         // Implementation of the function
//         None
//     }
//     fn remove_epoll(&mut self, epoll: &alloc::sync::Weak<crate::libs::spinlock::SpinLock<crate::net::event_poll::EventPoll>>) -> Result<(), SystemError> {
//         // Implementation of the function
//         Ok(())
//     }
//     fn clear_epoll(&mut self) -> Result<(), SystemError> {
//         // Implementation of the function
//         Ok(())
//     }
//     fn pool(&self) -> Option<crate::libs::pool::Pool> {
//         // Implementation of the function
//         None
//     }
//     fn ioctl(&self, _request: u32, _arg: u64) -> Result<u64, SystemError> {
//         // Implementation of the function
//         Ok(0)
//     }
//     fn metadata(&self) -> crate::net::socket::SocketMetadata {
//         // Implementation of the function
//         crate::net::socket::SocketMetadata::new()
//     }
//     fn box_clone(&self) -> Box<dyn Socket> {
//         // Implementation of the function
//         Box::new(NetlinkSock::new())
//     }
//     fn setsockopt(
//             &self,
//             _level: usize,
//             _optname: usize,
//             _optval: &[u8],
//         ) -> Result<(), SystemError> {
//         // Implementation of the function
//         Ok(())
//     }
//     fn socket_handle(&self) -> crate::net::socket::handle::GlobalSocketHandle {
//         // Implementation of the function
//         crate::net::socket::handle::GlobalSocketHandle::new()
//     }
//     fn write_buffer(&self, _buf: &[u8]) -> Result<usize, SystemError> {
//         // Implementation of the function
//         Ok(0)
//     }
//     fn as_any_ref(&self) -> &dyn Any {
//         // Implementation of the function
//         self
//     }
//     fn as_any_mut(&mut self) -> &mut dyn Any {
//         // Implementation of the function
//         self
//     }
//     fn add_epoll(&mut self, epitem: Arc<crate::net::event_poll::EPollItem>) -> Result<(), SystemError> {
//         // Implementation of the function
//         Ok(())
//     }
// }

// https://code.dragonos.org.cn/s?refs=netlink_create&project=linux-6.1.9
/// 创建一个netlink套接字
fn netlink_create(socket: &mut dyn Socket, protocol: i32, _kern: bool) -> Result<(), Error> {
    // 假设我们有一个类型来跟踪协议最大值
    const MAX_LINKS: i32 = 1024;

    // if socket.type_ != SocketType::Raw && socket.type_ != SocketType::Dgram {
    //     return Err(Error::SocketTypeNotSupported);
    // }
    if !(0..MAX_LINKS).contains(&protocol) {
        return Err(Error::ProtocolNotSupported);
    }
    // 安全的数组索引封装
    let protocol = protocol as usize;
    // 这里简化了锁和模块加载逻辑
    // 假设成功加载了模块和相关函数
    Ok(())
}

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
    masks: Vec<u64>,
}
impl listeners {
    fn new() -> listeners {
        listeners { masks: Vec::new() }
    }
    fn masks(&self) -> Vec<u64> {
        Vec::new()
    }
}

lazy_static! {
    /// 一个维护全局的NetlinkTable的哈希链，每一个元素代表一个netlink协议类型，最大数量为MAX_LINKS
    static ref NL_TABLE: RwLock<Vec<NetlinkTable>> = RwLock::new(vec![NetlinkTable::new()]);
}
pub fn netlink_has_listeners(sk: &NetlinkSock, group: i32) -> i32 {
    let mut res = 0;
    let nl_table = NL_TABLE.read();
    if let Some(listeners) = &nl_table[sk.sk_protocol()].listeners {
        if group - 1 < nl_table[sk.sk_protocol()].groups {
            res = listeners.masks[group as usize - 1] as i32;
        }
    }
    res
}
struct NetlinkBroadcastData<'a> {
    exclude_sk: &'a dyn NetlinkSocket,
    // net: &'a Net,
    portid: u32,
    group: u64,
    failure: i32,
    delivery_failure: i32,
    congested: i32,
    delivered: i32,
    allocation: u32,
    skb: Rc<RefCell<SkBuff<'a>>>,
    skb_2: Rc<RefCell<SkBuff<'a>>>,
}
impl<'a> NetlinkBroadcastData<'a> {
    pub fn copy_skb_to_skb_2(&mut self) {
        let skb = self.skb.borrow().clone_with_new_inner();
        *self.skb_2.borrow_mut() = skb;
    }
}
/// 弃用
fn sk_for_each_bound(sk: &NetlinkSock, mc_list: &HListHead) {
    let mut node = mc_list.first.as_ref();
    while let Some(n) = node {
        let data = &n.data;
        if data.sk_protocol() == sk.sk_protocol() {
            // Implementation of the function
        }
        node = n.next.as_ref();
    }
}
fn do_one_broadcast(sk: &Arc<dyn NetlinkSocket>, info: &mut Box<NetlinkBroadcastData>)->Result<(), SystemError> {
    let nlk: Arc<NetlinkSock> = Arc::clone(sk).arc_any().downcast().map_err(|_| SystemError::EINVAL)?;
    if info.exclude_sk.equals(&**sk) {
        return Err(SystemError::EINVAL);
    }
    if nlk.portid() == info.portid
        || info.group > nlk.ngroups()
        || !nlk.groups().contains(&(info.group - 1))
    {
        return Err(SystemError::EINVAL);
    }
    // TODO: 需要net namespace支持
    // if !net_eq(sock_net(sk), info.net) {
    //     if !(nlk.flags & NetlinkFlags::LISTEN_ALL_NSID.bits()) {
    //         return;
    //     }
    //     if !peernet_has_id(sock_net(sk), info.net) {
    //         return;
    //     }
    //     if !file_ns_capable(sk.sk_socket.file, info.net.user_ns, CAP_NET_BROADCAST) {
    //         return;
    //     }
    // }
    if info.failure != 0 {
        netlink_overrun(sk);
        return Err(SystemError::EINVAL);
    }
    sock_hold(sk);
    if info.skb_2.borrow().is_empty() {
        if skb_shared(&info.skb) {
            info.copy_skb_to_skb_2();
        } else {
            info.skb_2 = info.skb.clone();
            skb_orphan(&info.skb_2);
        }
        netlink_overrun(sk);
        info.failure = 1;
        if !sk.flags().is_zero() & !NetlinkFlags::BROADCAST_SEND_ERROR.bits().is_zero() {
            info.delivery_failure = 1;
        }
        return Err(SystemError::EINVAL);
    }
    if sk_filter(sk, &info.skb_2) {
        return Err(SystemError::EINVAL);
    }
    // TODO: 需要net namespace支持
    // peernet2id用于检索与给定网络(net)相关联的对等网络(peer)的ID
    // NETLINK_CB(info.skb_2).nsid = peernet2id(sock_net(sk), info.net);
    // if NETLINK_CB(info.skb_2).nsid != NETNSA_NSID_NOT_ASSIGNED {
    //     NETLINK_CB(info.skb_2).nsid_is_set = true;
    // }
    let ret: i32 = netlink_broadcast_deliver(sk, &mut info.skb_2);
    if ret < 0 {
        netlink_overrun(sk);
        if !sk.flags().is_zero() & !NetlinkFlags::BROADCAST_SEND_ERROR.bits().is_zero() {
            info.delivery_failure = 1;
        }
    } else {
        info.congested |= ret;
        info.delivered = 1;
        info.skb_2 = info.skb.clone();
    }
    sock_put(sk);
    Ok(())
}
pub fn netlink_broadcast<'a>(
    ssk: &'a NetlinkSock,
    skb: Rc<RefCell<SkBuff<'a>>>,
    portid: u32,
    group: u64,
    allocation: u32,
) -> Result<(), SystemError> {
    // TODO: 需要net namespace支持
    // let net = sock_net(ssk);
    let mut info = Box::new(NetlinkBroadcastData {
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
        skb_2: Rc::new(RefCell::new(SkBuff::new())),
    });

    // While we sleep in clone, do not allow to change socket list
    let nl_table = NL_TABLE.read();
    // 下面这行替代了sk_for_each_bound(ssk, &nl_table[ssk.sk_protocol()].mc_list);
    for sk in &mut nl_table[ssk.sk_protocol()].mc_list.iter() {
        let _ = do_one_broadcast(sk, &mut info);
    }

    consume_skb(info.skb);

    if info.delivery_failure != 0 {
        return Err(SystemError::ENOBUFS);
    }
    consume_skb(info.skb_2);

    if info.delivered != 0 {
        if info.congested != 0 {
            Syscall::do_sched_yield()?;
        }
        return Ok(());
    }
    return Err(SystemError::ESRCH);
}

// 对网络套接字(sk)和网络数据包(skb)进行过滤。
fn sk_filter(sk: &Arc<dyn NetlinkSocket>, skb: &Rc<RefCell<SkBuff>>) -> bool {
    // Implementation of the function
    false
}

// 处理Netlink套接字的广播消息传递
fn netlink_broadcast_deliver(sk: &Arc<dyn NetlinkSocket>, skb: &mut Rc<RefCell<SkBuff>>) -> i32 {
    // Implementation of the function
    0
}
