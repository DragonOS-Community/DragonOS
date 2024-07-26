//参考https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c


use core::mem::size_of;
use core::ops::DerefMut;
use core::{any::Any, cell::RefCell, fmt::Debug, hash::Hash, ops::Deref};

use alloc::borrow::ToOwned;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::sync::{Arc, Weak};

use hashbrown::HashMap;
use intertrait::CastFromSync;
use num::Zero;
use smoltcp::wire::IpListenEndpoint;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::libs::mutex::Mutex;
use crate::libs::rwlock::RwLockWriteGuard;
use crate::net::socket::netlink::skbuff::SkBuff;
use crate::{
    libs::rwlock::RwLock,
    net::{net_core::consume_skb, socket::SocketType},
    syscall::Syscall,
};
use alloc::{boxed::Box, vec::Vec};

use crate::net::socket::{AddressFamily, Socket};
use lazy_static::lazy_static;
use smoltcp::socket::raw::PacketBuffer;
use smoltcp::socket::raw::PacketMetadata;

use super::netlink::{NETLINK_USERSOCK, NL_CFG_F_NONROOT_SEND};
use super::netlink_proto::{proto_register, Proto, NETLINK_PROTO};
use super::skbuff::{netlink_overrun, skb_orphan, skb_shared};

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
const NETLINK_S_CONGESTED: u64 = 0x0;
pub struct SockaddrNl {
    // pub nl_family: SA_FAMILY_T,
    pub nl_pad: u16,
    pub nl_pid: u32,
    pub nl_groups: u32,
}
#[derive(Clone)]
#[derive(Debug)]
pub struct HListHead {
    first: Option<Arc<HListNode>>,
}
#[derive(Debug)]
pub struct HListNode {
    data: Arc<Mutex<Box<dyn NetlinkSocket>>>,
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
    type Item = &'a Arc<Mutex<Box<dyn NetlinkSocket>>>;

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
#[derive(Clone)]
pub struct NetlinkTable {
    hash: HashMap<u32, Arc<dyn NetlinkSocket>>,
    listeners: Option<listeners>,
    registered: u32,
    flags: u32,
    groups: i32,
    mc_list: HListHead,
}
impl<'a> NetlinkTable {
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
    sock_register(&NETLINK_FAMILY_OPS);
    // register_pernet_subsys(&netlink_net_ops);
    // register_pernet_subsys(&netlink_tap_net_ops);
    /* The netlink device handler may be needed early. */
    // rtnetlink_init();
    Ok(())
}

/// 
pub trait NetProtoFamily {
    fn create(socket: &mut dyn Socket, protocol: i32, _kern: bool) -> Result<(), Error>;
}
/// 
pub struct NetlinkFamulyOps {
    family: AddressFamily,
    // owner: Module,
}

impl NetProtoFamily for NetlinkFamulyOps {
    // https://code.dragonos.org.cn/s?refs=netlink_create&project=linux-6.1.9
    /// netlink_create() 创建一个netlink套接字
    fn create(socket: &mut dyn Socket, protocol: i32, _kern: bool) -> Result<(), Error> {
        // 假设我们有一个类型来跟踪协议最大值
        const MAX_LINKS: i32 = 1024;
        // if socket.type_ != SocketType::Raw && socket.type_ != SocketType::Dgram {
        //     return Err(Error::SocketTypeNotSupported);
        // }
        if !(0..MAX_LINKS).contains(&protocol) {
            // todo: 这里不符合规范，后续待修改为 SystemError
            return Err(Error::ProtocolNotSupported);
        }
        // 安全的数组索引封装
        let protocol = protocol as usize;
        // 这里简化了锁和模块加载逻辑
        // 假设成功加载了模块和相关函数
        Ok(())
    }
}

lazy_static! {
    static ref NETLINK_FAMILY_OPS: NetlinkFamulyOps = NetlinkFamulyOps {
        family: AddressFamily::Netlink,
    };
}

pub fn sock_register(ops: &NetlinkFamulyOps) {

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
// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#572
/// 内核套接字插入 nl_table
fn netlink_insert(sk: Arc<dyn NetlinkSocket>, portid: u32) -> Result<(), SystemError> {
    let mut nl_table = NL_TABLE.write();
    let index = Arc::clone(&sk).sk_protocol();
   

    let nlk: Arc<LockedNetlinkSock> = Arc::clone(&sk).arc_any().downcast().map_err(|_| SystemError::EINVAL)?;

    {
        let nlk_guard = nlk.0.read();
        // 检查端口是否已经被绑定
        if nlk_guard.portid != portid {
            return Err(SystemError::EOVERFLOW);
        }
        // 检查套接字是否已经绑定
        if nlk_guard.bound {
            log::debug!("netlink_insert: socket already bound\n");
            return Err(SystemError::EADDRINUSE);
        }
    }

    {
        let mut nlk_guard = nlk.0.write();
        // 绑定端口
        nlk_guard.portid = portid;
        // 设置套接字已绑定
        nlk_guard.bound = true;
        // 将套接字插入哈希表
        nl_table[index].hash.insert(portid, Arc::clone(&sk));
    }
    
    Ok(())
}

// TODO: net namespace支持
// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#532
/// 在 netlink_table 中查找 netlink 套接字
fn netlink_lookup(protocol:i32, portid:u32)-> Arc<dyn NetlinkSocket>{
    let nl_table = NL_TABLE.read();
    let index = protocol as usize;
    let sk = nl_table[index].hash.get(&portid).unwrap();
    Arc::clone(sk)
}

// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#672

pub enum Error {
    SocketTypeNotSupported,
    ProtocolNotSupported,
}

// netlink机制特定的内核抽象，不同于标准的trait Socket
pub trait NetlinkSocket: Any + Send + Sync + Debug + CastFromSync {
    // fn sk_prot(&self) -> &dyn proto;
    fn sk_family(&self) -> i32;
    fn sk_state(&self) -> i32;
    fn sk_protocol(&self) -> usize;
    fn sk_rmem_alloc(&self) -> usize;
    fn sk_rcvbuf(&self) -> usize;
    fn enqueue_skb(&mut self, skb: Arc<RwLock<SkBuff>>);
    /// 数据就绪回调
    fn sk_data_ready(&self);
    fn is_kernel(&self) -> bool;
    fn equals(&self, other: Option<Arc<Mutex<Box<dyn NetlinkSocket>>>>) -> bool;
    fn clone_box(&self) -> Box<dyn NetlinkSocket>;
    fn portid(&self) -> u32;
    fn ngroups(&self) -> u64;
    fn groups(&self) -> Vec<u64>;
    fn flags(&self) -> u32;
}
impl Clone for Box<dyn NetlinkSocket> {
    fn clone(&self) -> Box<dyn NetlinkSocket> {
        self.clone_box()
    }
}
/* linux：struct sock has to be the first member of netlink_sock */
// linux 6.1.9中的netlink_sock结构体里，sock是一个很大的结构体，这里简化
// 意义是netlink_sock（NetlinkSock）是一个sock（NetlinkSocket）

pub struct LockedNetlinkSock(RwLock<NetlinkSock>);
impl LockedNetlinkSock {
    pub fn new(netlinksock: NetlinkSock) -> LockedNetlinkSock {
        LockedNetlinkSock(RwLock::new(netlinksock))
    }
}
#[derive(Debug)]
pub struct NetlinkSock {
    sk: Option<Weak<dyn NetlinkSocket>>,
    portid: u32,
    node: Arc<HListHead>,
    dst_portid: u32,
    dst_group: u32,
    flags: u32,
    subscriptions: u32,
    ngroups: u64,
    groups: Vec<u64>,
    bound: bool,
    state: u64,
    max_recvmsg_len: usize,
    dump_done_errno: i32,
    cb_running: bool,
    queue: Vec<Arc<RwLock<SkBuff>>>,
}
// TODO: 实现NetlinkSocket trait
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
    fn sk_rmem_alloc(&self) -> usize {
        0
    }
    fn sk_rcvbuf(&self) -> usize {
        0
    }
    fn enqueue_skb(&mut self, skb: Arc<RwLock<SkBuff>>) {
        self.queue.push(skb);
    }
    fn sk_data_ready(&self) {
        // Implementation of the function
        
    }
    fn is_kernel(&self) -> bool {
        true
    }
    fn equals(&self, other: Option<Arc<Mutex<Box<dyn NetlinkSocket>>>>) -> bool {
        if let Some(self_sk) = self.sk.as_ref().unwrap().upgrade() {
            self_sk.equals(other)
        } else {
            false
        }
    }
    fn clone_box(&self) -> Box<dyn NetlinkSocket> {
        Box::new(NetlinkSock {
            sk: self.sk.clone(),
            portid: self.portid,
            node: self.node.clone(),
            dst_portid: self.dst_portid,
            dst_group: self.dst_group,
            flags: self.flags,
            subscriptions: self.subscriptions,
            ngroups: self.ngroups,
            groups: self.groups.clone(),
            bound: self.bound,
            state: self.state,
            max_recvmsg_len: self.max_recvmsg_len,
            dump_done_errno: self.dump_done_errno,
            cb_running: self.cb_running,
            queue: self.queue.clone(),
        })
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
// impl PartialEq for NetlinkSock {
//     fn eq(&self, other: &Self) -> bool {
//         if let Some(self_sk) = self.sk.as_ref().unwrap().upgrade() {
//            if let Some(other_sk) = other.sk.as_ref() {
//                self_sk.equals(other_sk.upgrade())
//            } else {
//                false
//            }
//         } else {
//             false
//         }
//     }
// }
// impl Deref for NetlinkSock {
//     type Target = Arc<dyn NetlinkSocket>;

//     fn deref(&self) -> &Self::Target {
//         self.sk.as_ref().unwrap().upgrade().unwrap().deref()
//     }
// }
// impl DerefMut for NetlinkSock {
//     fn deref_mut(&mut self) -> &mut Self::Target {
//         self.sk.as_ref().unwrap().upgrade()
//     }
// }
impl NetlinkSock {
    pub fn new() -> NetlinkSock {
        NetlinkSock {
            sk: None,
            portid: 0,
            node: Arc::new(HListHead { first: None }),
            dst_portid: 0,
            dst_group: 0,
            flags: 0,
            subscriptions: 0,
            ngroups: 0,
            groups: Vec::new(),
            bound: false,
            state: 0,
            max_recvmsg_len: 0,
            dump_done_errno: 0,
            cb_running: false,
            queue: Vec::new(),
        }
    }
    pub fn get_sk(&self) -> &Weak<dyn NetlinkSocket> {
        self.sk.as_ref().unwrap()
    }
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
#[derive(Clone)]
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
    /// 一个维护全局的 NetlinkTable 哈希链的向量，每一个元素代表一个 netlink 协议类型，最大数量为MAX_LINKS
    static ref NL_TABLE: RwLock<Vec<NetlinkTable>> = RwLock::new(vec![NetlinkTable::new(); MAX_LINKS]);
}
pub fn netlink_has_listeners(sk: &Arc<dyn NetlinkSocket>, group: i32) -> i32 {
    let mut res = 0;
    let protocol = sk.sk_protocol();
    let nl_table = NL_TABLE.read();
    if let Some(listeners) = &nl_table[protocol].listeners {
        if group - 1 < nl_table[protocol].groups {
            res = listeners.masks[group as usize - 1] as i32;
        }
    }
    res
}
struct NetlinkBroadcastData<'a> {
    exclude_sk: &'a Arc<dyn NetlinkSocket>,
    // net: &'a Net,
    portid: u32,
    group: u64,
    failure: i32,
    delivery_failure: i32,
    congested: i32,
    delivered: i32,
    allocation: u32,
    skb: Arc<RwLock<SkBuff>>,
    skb_2: Arc<RwLock<SkBuff>>,
}
impl<'a> NetlinkBroadcastData<'a> {
    pub fn copy_skb_to_skb_2(&mut self) {
        let skb = self.skb.read().clone();
        *self.skb_2.write() = skb;
    }
}
/// 尝试向指定用户进程 netlink 套接字发送组播消息
/// ## 参数：
/// - sk: 指向一个 sock 结构，对应一个用户进程 netlink 套接字
/// - info: 指向一个 netlink 组播消息的管理块
/// ## 备注：
/// 传入的 netlink 套接字跟组播消息属于同一种 netlink 协议类型，并且这个套接字开启了组播阅订，除了这些，其他信息(比如阅订了具体哪些组播)都是不确定的
fn do_one_broadcast(sk: Arc<Mutex<Box<dyn NetlinkSocket>>>, info: &mut Box<NetlinkBroadcastData>)->Result<(), SystemError> {
    // 从Arc<dyn NetlinkSocket>中获取NetlinkSock
    let nlk: Arc<NetlinkSock> = Arc::clone(&sk).arc_any().downcast().map_err(|_| SystemError::EINVAL)?;
    // 如果源 sock 和目的 sock 是同一个则直接返回
    if info.exclude_sk.equals(Some(sk.clone())) {
        return Err(SystemError::EINVAL);
    }
    // 如果目的单播地址就是该 netlink 套接字
    // 或者目的组播地址超出了该 netlink 套接字的上限
    // 或者该 netlink 套接字没有阅订这条组播消息，都直接返回
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

    // 如果 netlink 组播消息的管理块携带了 failure 标志, 则对该 netlink 套接字设置缓冲区溢出状态
    if info.failure != 0 {
        netlink_overrun(&sk);
        return Err(SystemError::EINVAL);
    }
    // 设置 skb2，其内容来自 skb
    if info.skb_2.read().is_empty() {
        if skb_shared(&info.skb) {
            info.copy_skb_to_skb_2();
        } else {
            info.skb_2 = Arc::new(RwLock::new(info.skb.read().clone()));
            skb_orphan(&info.skb_2);
        }
    }
    // 到这里如果 skb2 还是 NULL，意味着上一步中 clone 失败
    if info.skb_2.read().is_empty() {
        netlink_overrun(&sk);
        info.failure = 1;
        if !sk.lock().flags().is_zero() & !NetlinkFlags::BROADCAST_SEND_ERROR.bits().is_zero() {
            info.delivery_failure = 1;
        }
        return Err(SystemError::EINVAL);
    }
    if sk_filter(&sk, &info.skb_2) {
        return Err(SystemError::EINVAL);
    }
    // TODO: 需要net namespace支持
    // peernet2id用于检索与给定网络(net)相关联的对等网络(peer)的ID
    // NETLINK_CB(info.skb_2).nsid = peernet2id(sock_net(sk), info.net);
    // if NETLINK_CB(info.skb_2).nsid != NETNSA_NSID_NOT_ASSIGNED {
    //     NETLINK_CB(info.skb_2).nsid_is_set = true;
    // }
    let ret = netlink_broadcast_deliver(Arc::clone(&sk), &info.skb_2);
    // 如果将承载了组播消息的 skb 发送到该用户进程 netlink 套接字失败
    if ret < 0 {
        netlink_overrun(&sk);
        if !sk.lock().flags().is_zero() & !NetlinkFlags::BROADCAST_SEND_ERROR.bits().is_zero() {
            info.delivery_failure = 1;
        }
    } else {
        info.congested |= ret;
        info.delivered = 1;
        info.skb_2 = Arc::new(RwLock::new(info.skb.read().clone()));
    }
    drop(sk);
    Ok(())
}
/// 发送 netlink 组播消息
/// ## 参数
/// - ssk: 源 sock
/// - skb: 属于发送方的承载了netlink消息的skb
/// - portid: 目的单播地址
/// - group: 目的组播地址
///
/// ## 备注: 以下2种情况都会调用到本函数：
///  [1]. 用户进程   --组播--> 用户进程
///  [2]. kernel     --组播--> 用户进程
///
pub fn netlink_broadcast<'a>(
    ssk: &'a Arc<dyn NetlinkSocket>,
    skb: Arc<RwLock<SkBuff>>,
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
        skb_2: Arc::new(RwLock::new(SkBuff::new())),
    });

    // While we sleep in clone, do not allow to change socket list
    let nl_table = NL_TABLE.read();
    // 遍历该 netlink 套接字所在协议类型中所有阅订了组播功能的套接字，然后尝试向其发送该组播消息
    for sk in &mut nl_table[ssk.sk_protocol()].mc_list.iter() {
        let _ = do_one_broadcast(Arc::clone(sk), &mut info);
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

/// 对网络套接字(sk)和网络数据包(skb)进行过滤
fn sk_filter(sk: &Arc<Mutex<Box<dyn NetlinkSocket>>>, skb: &Arc<RwLock<SkBuff>>) -> bool {
    // TODO: Implementation of the function
    false
}

// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c?fi=netlink_has_listeners#1400
/// 处理Netlink套接字的广播消息传递
/// - 将携带了 netlink 组播消息的 skb 发送到指定目的用户进程 netlink 套接字
/// 
/// ## 参数
/// - sk: 指向一个 sock 结构，对应一个用户进程 netlink 套接字
/// - skb: 指向一个网络缓冲区 skb，携带了 netlink 组播消息
/// 
/// ## 返回值      
///  - -1: 套接字接收条件不满足
///  - 0: netlink组播消息发送成功，套接字已经接收但尚未处理数据长度小于等于其接收缓冲的1/2
///  - 1: netlink组播消息发送成功，套接字已经接收但尚未处理数据长度大于其接收缓冲的1/2(这种情况似乎意味着套接字处于拥挤状态)
///
/// ## 备注：
/// - 到这里，已经确定了传入的 netlink 套接字跟组播消息匹配正确；
/// - netlink 组播消息不支持阻塞
fn netlink_broadcast_deliver(sk: Arc<Mutex<Box<dyn NetlinkSocket>>>, skb: &Arc<RwLock<SkBuff>>) -> i32 {
    let nlk: Arc<LockedNetlinkSock> = Arc::clone(&sk).arc_any().downcast().expect("Invalid downcast to LockedNetlinkSock");
    let nlk_guard = nlk.0.read();
    // 如果接收缓冲区的已分配内存小于或等于其总大小，并且套接字没有被标记为拥塞，则继续执行内部的代码块。
    if (sk.lock().sk_rmem_alloc()<= sk.lock().sk_rcvbuf()) && !(nlk_guard.state == NETLINK_S_CONGESTED) {
        // 如果满足接收条件，则设置skb的所有者是该netlink套接字
        netlink_skb_set_owner_r(skb, sk.clone());
        // 将 skb 发送到该 netlink 套接字，实际也就是将该 skb 放入了该套接字的接收队列中
        let _ = netlink_sendskb(sk.clone(), skb);
        // 如果套接字的接收缓冲区已经接收但尚未处理数据长度大于其接收缓冲的1/2，则返回1
        if &sk.lock().sk_rmem_alloc() > &((sk.lock().sk_rcvbuf() >> 1)){
            return 1;
        }else{
            return 0;
        }
    }
    -1
}
// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c?fi=netlink_has_listeners#387
/// 设置一个网络缓冲区skb的所有者为指定的源套接字sk
fn netlink_skb_set_owner_r(skb: &Arc<RwLock<SkBuff>>, sk: Arc<Mutex<Box<dyn NetlinkSocket>>>) {
    // WARN_ON(skb->sk != NULL);
    let mut skb_write = skb.write();
    skb_write.sk = sk;
    // skb->destructor = netlink_skb_destructor;
    // atomic_add(skb->truesize, &sk->sk_rmem_alloc);
    // sk_mem_charge(sk, skb->truesize);
}
pub struct NetlinkSocketWrapper {
    sk: Arc<dyn NetlinkSocket>,
}
impl NetlinkSocketWrapper {
    pub fn new(sk: Arc<dyn NetlinkSocket>) -> NetlinkSocketWrapper {
        NetlinkSocketWrapper { sk }
    }
}
// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c?fi=netlink_has_listeners#1268
/// 将一个网络缓冲区 skb 中的数据发送到指定的 目标进程套接字 sk
fn netlink_sendskb(sk: Arc<Mutex<Box<dyn NetlinkSocket>>>, skb: &Arc<RwLock<SkBuff>>)->u32 {
    let len = skb.read().len;
    {
        // 将 skb 放入该 netlink 套接字接收队列末尾
        sk.lock().enqueue_skb(skb.clone());
        // 执行 sk_data_ready 回调通知该套接字有数据可读
        sk.lock().sk_data_ready();
    }
    len
}
// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#1337
/// 
fn netlink_unicast(){

}

// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#1316
///
fn netlink_unicast_kernel(){
    
}