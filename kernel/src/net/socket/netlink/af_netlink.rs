// 参考https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c
use crate::filesystem::vfs::{FilePrivateData, FileSystem, IndexNode};
use crate::libs::mutex::{Mutex, MutexGuard};
use crate::libs::rwlock::RwLockWriteGuard;
use crate::libs::spinlock::SpinLockGuard;
use crate::net::socket::netlink::skbuff::SkBuff;
use crate::net::socket::*;
use crate::net::syscall::SockAddrNl;
use crate::time::timer::schedule_timeout;
use crate::{libs::rwlock::RwLock, syscall::Syscall};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::{boxed::Box, vec::Vec};
use core::mem;
use core::ops::Deref;
use core::ptr::copy_nonoverlapping;
use core::{any::Any, fmt::Debug, hash::Hash};
use hashbrown::HashMap;
use intertrait::CastFromSync;
use netlink::{
    sk_data_ready, NetlinkKernelCfg, NETLINK_ADD_MEMBERSHIP, NETLINK_DROP_MEMBERSHIP,
    NETLINK_PKTINFO,
};
use num::Zero;
use system_error::SystemError;
use system_error::SystemError::ECONNREFUSED;
use unified_init::macros::unified_init;

use crate::net::socket::{AddressFamily, Endpoint, Inode, MessageFlag, Socket};
use lazy_static::lazy_static;

use super::callback::NetlinkCallback;
use super::endpoint::NetlinkEndpoint;
use super::netlink_proto::{proto_register, NETLINK_PROTO};
use super::skbuff::{netlink_overrun, skb_orphan, skb_shared};
use super::sock::SockFlags;
use super::{NLmsgFlags, NLmsgType, NLmsghdr, VecExt, NETLINK_USERSOCK, NL_CFG_F_NONROOT_SEND};
use crate::init::initcall::INITCALL_CORE;
use crate::net::socket::netlink::NetlinkState;
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
#[derive(Clone, Debug)]
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
type NetlinkSockComparator = Arc<dyn Fn(&NetlinkSock) -> bool + Send + Sync>;
/// 每一个netlink协议族都有一个NetlinkTable，用于保存该协议族的所有netlink套接字
pub struct NetlinkTable {
    hash: HashMap<u32, Arc<Mutex<Box<dyn NetlinkSocket>>>>,
    listeners: Option<Listeners>,
    registered: u32,
    flags: u32,
    groups: u32,
    mc_list: HListHead,
    pub bind: Option<Arc<dyn Fn(i32) -> i32 + Send + Sync>>,
    pub unbind: Option<Arc<dyn Fn(i32) -> i32 + Send + Sync>>,
    pub compare: Option<NetlinkSockComparator>,
}
impl NetlinkTable {
    fn new() -> NetlinkTable {
        NetlinkTable {
            hash: HashMap::new(),
            listeners: Some(Listeners {
                masks: Vec::with_capacity(32),
            }),
            registered: 0,
            flags: 0,
            groups: 32,
            mc_list: HListHead { first: None },
            bind: None,
            unbind: None,
            compare: None,
        }
    }
    fn listeners(&self) -> Listeners {
        Listeners::new()
    }
    fn flags(&self) -> u32 {
        0
    }
    fn groups(&self) -> u32 {
        0
    }
    pub fn set_registered(&mut self, registered: u32) {
        self.registered = registered;
    }
    pub fn set_flags(&mut self, flags: u32) {
        self.flags = flags;
    }
    pub fn set_groups(&mut self, groups: u32) {
        self.groups = groups;
    }
    pub fn get_registered(&self) -> u32 {
        self.registered
    }
    fn set_callbacks(&mut self, cfg: NetlinkKernelCfg) {
        self.bind = cfg.bind;
        self.unbind = cfg.unbind;
        self.compare = cfg.compare;
    }
}

// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#2916
/// netlink 协议的最大数量
const MAX_LINKS: usize = 32;
#[unified_init(INITCALL_CORE)]
/// netlink 协议的初始化函数
fn netlink_proto_init() -> Result<(), SystemError> {
    unsafe {
        let err = proto_register(&mut NETLINK_PROTO, 0);
        if err.is_err() {
            return Err(SystemError::ENOSYS);
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

pub struct NetlinkFamulyOps {
    family: AddressFamily,
    // owner: Module,
}

// impl NetProtoFamily for NetlinkFamulyOps {
//     // https://code.dragonos.org.cn/s?refs=netlink_create&project=linux-6.1.9
//     /// netlink_create() 用户空间创建一个netlink套接字
//     fn create(socket: &mut dyn Socket, protocol: i32, _kern: bool) -> Result<(), Error> {
//         // 假设我们有一个类型来跟踪协议最大值
//         const MAX_LINKS: i32 = 1024;
//         // if socket.type_ != SocketType::Raw && socket.type_ != SocketType::Dgram {
//         //     return Err(Error::SocketTypeNotSupported);
//         // }
//         if !(0..MAX_LINKS).contains(&protocol) {
//             // todo: 这里不符合规范，后续待修改为 SystemError
//             return Err(Error::ProtocolNotSupported);
//         }
//         // 安全的数组索引封装
//         let protocol = protocol as usize;
//         Ok(())
//     }
// }

lazy_static! {
    static ref NETLINK_FAMILY_OPS: NetlinkFamulyOps = NetlinkFamulyOps {
        family: AddressFamily::Netlink,
    };
}

pub fn sock_register(ops: &NetlinkFamulyOps) {}
/// 初始化和注册一个用户套接字条目，并将其添加到全局的NetlinkTable向量中
pub fn netlink_add_usersock_entry(nl_table: &mut RwLockWriteGuard<Vec<NetlinkTable>>) {
    let listeners: Option<Listeners> = Some(Listeners::new());
    let groups: u32 = 32;
    if listeners.is_none() {
        panic!("netlink_add_usersock_entry: Cannot allocate listeners\n");
    }

    let index = NETLINK_USERSOCK;
    nl_table[index].groups = groups;
    log::debug!(
        "netlink_add_usersock_entry: nl_table[index].groups: {}",
        nl_table[index].groups
    );
    // rcu_assign_pointer(nl_table[index].listeners, listeners);
    // nl_table[index].module = THIS_MODULE;
    nl_table[index].registered = 1;
    nl_table[index].flags = NL_CFG_F_NONROOT_SEND;
}
// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#572
/// 内核套接字插入 nl_table
pub fn netlink_insert(
    sk: Arc<Mutex<Box<dyn NetlinkSocket>>>,
    portid: u32,
) -> Result<(), SystemError> {
    let mut nl_table: RwLockWriteGuard<Vec<NetlinkTable>> = NL_TABLE.write();

    let index = sk.lock().sk_protocol();

    let nlk = Arc::new(RwLock::new(
        sk.lock()
            .deref()
            .as_any()
            .downcast_ref::<NetlinkSock>()
            .ok_or(SystemError::EINVAL)?
            .clone(),
    ));
    {
        let nlk_guard = nlk.read();
        // 检查端口是否匹配
        if nlk_guard.portid != portid {
            log::debug!("netlink_insert: portid mismatch\n");
            return Err(SystemError::EOVERFLOW);
        }
    }

    {
        let mut nlk_guard = nlk.write();
        // 绑定端口
        nlk_guard.portid = portid;
        // 设置套接字已绑定
        nlk_guard.bound = portid != 0;
        // 将套接字插入哈希表
        nl_table[index].hash.insert(portid, Arc::clone(&sk));
        log::debug!("netlink_insert: inserted socket\n");
    }

    Ok(())
}

fn netlink_bind(
    sock: Arc<Mutex<Box<dyn NetlinkSocket>>>,
    addr: &SockAddrNl,
) -> Result<(), SystemError> {
    log::info!("netlink_bind here!");
    let sk: Arc<Mutex<Box<dyn NetlinkSocket>>> = Arc::clone(&sock);
    // todo: net namespace支持
    // let net = sock_net(sk);
    log::info!("netlink_bind: nl_family: {:?}", addr.nl_family);
    // let nlk: Arc<Mutex<NetlinkSock>> = sock
    //     .clone()
    //     .arc_any()
    //     .downcast()
    //     .map_err(|_| SystemError::EINVAL)?;

    let nlk = Arc::new(Mutex::new(
        sock.lock()
            .deref()
            .as_any()
            .downcast_ref::<NetlinkSock>()
            .ok_or(SystemError::EINVAL)?
            .clone(),
    ));
    let nladdr = addr;
    let mut groups: u32;
    log::info!("netlink_bind: nl_family: {:?}", nladdr.nl_family);
    if nladdr.nl_family != AddressFamily::Netlink {
        log::warn!("netlink_bind: nl_family != AF_NETLINK");
        return Err(SystemError::EINVAL);
    }
    groups = nladdr.nl_groups;
    log::info!("netlink_bind: groups: {}", groups);
    let mut nlk = nlk.lock();
    // Only superuser is allowed to listen multicasts
    if groups != 0 {
        let group_count = addr.nl_groups.count_ones(); // 计算多播组数量
        nlk.ngroups = group_count;
        // if !netlink_allowed(sock, NL_CFG_F_NONROOT_RECV) {
        //     return Err(-EPERM);
        // }
        let _ = netlink_realloc_groups(&mut nlk);
    }

    // BITS_PER_LONG = __WORDSIZE = 64
    if nlk.ngroups < 64 {
        groups &= (1 << nlk.ngroups) - 1;
    }

    let bound = nlk.bound;
    log::info!("netlink_bind: bound: {}", bound);
    if bound {
        // Ensure nlk.portid is up-to-date.
        if nladdr.nl_pid != nlk.portid {
            return Err(SystemError::EINVAL);
        }
    }

    if groups != 0 {
        for group in 0..(mem::size_of::<u32>() * 8) as u32 {
            if group != groups {
                continue;
            }
            // 尝试绑定到第 group + 1 个组播组。如果绑定成功（错误码err为0），则继续绑定下一个组播组。
            // err = nlk.bind().unwrap()(group + 1);
            // if err == 0 {
            //     continue;
            // }
            // netlink_undo_bind(group, groups, sk);
            // return Err(SystemError::EINVAL);
        }
    }

    // No need for barriers here as we return to user-space without
    // using any of the bound attributes.
    if !bound {
        if nladdr.nl_pid != 0 {
            log::info!("netlink_bind: insert");
            let _ = netlink_insert(sk, nladdr.nl_pid);
        } else {
            log::info!("netlink_bind: autobind");
            netlink_autobind(sock, &mut nlk.portid);
        };
        // if err != 0 {
        // BITS_PER_TYPE<TYPE> = SIZEOF TYPE * BITS PER BYTES
        // todo
        // netlink_undo_bind(mem::size_of::<u32>() * 8, groups, sk);
        // netlink_unlock_table();
        //     return Err(SystemError::EINVAL);
        // }
    }
    // todo
    // netlink_update_subscriptions(sk, nlk.subscriptions + hweight32(groups) - hweight32(nlk.groups.unwrap()[0]));
    log::info!("netlink_bind: nlk.groups: {:?}", nlk.groups);
    nlk.groups[0] = groups;
    log::info!("netlink_bind: nlk.groups: {:?}", nlk.groups);
    netlink_update_listeners(nlk);

    Ok(())
}

/// 自动为netlink套接字选择一个端口号，并在netlink table 中插入这个端口。如果端口已经被使用，它会尝试使用不同的端口号直到找到一个可用的端口。如果有多个线程同时尝试绑定，则认为是正常情况，并成功返回.
fn netlink_autobind(sk: Arc<Mutex<Box<dyn NetlinkSocket>>>, portid: &mut u32) {
    let mut rover: u32 = 0;
    loop {
        // 假设 netlink_lookup 是一个函数，返回一个 Option<Arc<Mutex<Box<dyn NetlinkSocket>>>> 类型
        let ret = netlink_lookup(sk.lock().sk_protocol(), *portid);

        // 如果查询成功
        if ret.is_some() {
            // 如果 rover 是 0，重置为 1
            if rover == 0 {
                // todo：随机
                rover = 1; // 在 Rust 中不能有 -4096 这样的u32值，因此我们从 1 开始递减
            } else {
                // 否则递减 rover
                rover -= 1;
            }
            *portid = rover;
        } else {
            // 如果查询失败，增加 rover
            rover += 1;
            *portid = rover;
            break;
        }
    }
    let _ = netlink_insert(sk, *portid);
}
// TODO: net namespace支持
// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#532
/// 在 netlink_table 中查找 netlink 套接字
fn netlink_lookup(protocol: usize, portid: u32) -> Option<Arc<Mutex<Box<dyn NetlinkSocket>>>> {
    // todo: net 支持
    let nl_table = NL_TABLE.read();
    let index = protocol;
    let sk = nl_table[index].hash.get(&portid).unwrap();
    Some(Arc::clone(sk))
}

// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#672

// netlink机制特定的内核抽象，不同于标准的trait Socket
pub trait NetlinkSocket: Socket + Any {
    // fn sk_prot(&self) -> &dyn proto;
    fn sk_family(&self) -> i32;
    fn sk_state(&self) -> NetlinkState;
    fn sk_protocol(&self) -> usize;
    fn sk_rmem_alloc(&self) -> usize;
    fn sk_rcvbuf(&self) -> usize;
    fn enqueue_skb(&mut self, skb: Arc<RwLock<SkBuff>>);
    fn is_kernel(&self) -> bool;
    fn equals(&self, other: Arc<Mutex<Box<dyn NetlinkSocket>>>) -> bool;
    fn portid(&self) -> u32;
    fn ngroups(&self) -> u64;
    fn groups(&self) -> Vec<u64>;
    fn flags(&self) -> Option<SockFlags>;
    fn sock_sndtimeo(&self, noblock: bool) -> i64;
    fn as_any(&self) -> &dyn Any;
}

pub trait NetlinkSocketWithCallback {
    fn sk_data_ready(&self, callback: impl Fn(i32) -> i32);
}
/* linux：struct sock has to be the first member of netlink_sock */
// linux 6.1.9中的netlink_sock结构体里，sock是一个很大的结构体，这里简化
// 意义是：netlink_sock（NetlinkSock）是一个sock（NetlinkSocket）, 实现了 Netlinksocket trait 和 Sock trait.

#[derive(Debug, Clone)]
struct NetlinkSockMetadata {}
impl NetlinkSockMetadata {
    fn new() -> NetlinkSockMetadata {
        NetlinkSockMetadata {}
    }
}
#[derive(Debug, Clone)]
#[cast_to([sync] Socket)]
#[cast_to([sync] NetlinkSocket)]
pub struct NetlinkSock {
    // sk: Option<Weak<dyn NetlinkSocket>>,
    portid: u32,
    node: Arc<HListHead>,
    dst_portid: u32,
    dst_group: u32,
    pub flags: u32,
    subscriptions: u32,
    ngroups: u32,
    groups: Vec<u32>,
    pub protocol: usize,
    bound: bool,
    state: NetlinkState,
    max_recvmsg_len: usize,
    dump_done_errno: i32,
    cb_running: bool,
    queue: Vec<Arc<RwLock<SkBuff>>>,
    data: Arc<Mutex<Vec<Vec<u8>>>>,
    sk_sndtimeo: i64,
    sk_rcvtimeo: i64,
    callback: Option<&'static dyn NetlinkCallback>,
}
impl Socket for NetlinkSock {
    fn connect(&self, _endpoint: Endpoint) -> Result<(), SystemError> {
        self.netlink_connect(_endpoint)
    }
    fn shutdown(&self, _type: ShutdownTemp) -> Result<(), SystemError> {
        todo!()
    }
    fn bind(&self, _endpoint: Endpoint) -> Result<(), SystemError> {
        log::debug!("NetlinkSock bind to {:?}", _endpoint);
        match _endpoint {
            Endpoint::Netlink(netlinkendpoint) => {
                let addr = netlinkendpoint.addr;
                let sock: Arc<Mutex<Box<dyn NetlinkSocket>>> =
                    Arc::new(Mutex::new(Box::new(self.clone())));
                return netlink_bind(sock, &addr);
            }
            _ => {
                return Err(SystemError::EINVAL);
            }
        }
    }
    fn close(&self) -> Result<(), SystemError> {
        Ok(())
    }
    fn listen(&self, _backlog: usize) -> Result<(), SystemError> {
        todo!()
    }
    fn accept(&self) -> Result<(Arc<Inode>, Endpoint), SystemError> {
        todo!()
    }

    fn wait_queue(&self) -> &WaitQueue {
        todo!()
    }

    fn poll(&self) -> usize {
        todo!()
    }
    // 借用 send_to 的接口模拟netlink_sendmsg的功能
    fn send_to(
        &self,
        buffer: &[u8],
        flags: MessageFlag,
        address: Endpoint,
    ) -> Result<usize, SystemError> {
        log::debug!("NetlinkSock send_to");
        return self.netlink_send(buffer, address);
    }
    // 借用 recv_from 的接口模拟netlink_recvmsg的功能
    fn recv_from(
        &self,
        msg: &mut [u8],
        flags: MessageFlag,
        address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        log::debug!("NetlinkSock recv_from");
        return self.netlink_recv(msg, flags);
    }
    fn send_buffer_size(&self) -> usize {
        log::warn!("send_buffer_size is implemented to 0");
        0
    }
    fn recv_buffer_size(&self) -> usize {
        log::warn!("recv_buffer_size is implemented to 0");
        0
    }

    fn set_option(&self, level: OptionsLevel, name: usize, val: &[u8]) -> Result<(), SystemError> {
        return netlink_setsockopt(self, level, name, val);
    }
}
impl IndexNode for NetlinkSock {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Implementation of the function
        Ok(0)
    }
    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Implementation of the function
        Ok(0)
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        todo!()
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn list(&self) -> Result<Vec<String>, SystemError> {
        // Implementation of the function
        Ok(Vec::new())
    }
}
// TODO: 实现 NetlinkSocket trait
impl NetlinkSocket for NetlinkSock {
    fn sk_family(&self) -> i32 {
        0
    }
    fn sk_state(&self) -> NetlinkState {
        return self.state;
    }
    fn sk_protocol(&self) -> usize {
        return self.protocol;
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
    fn is_kernel(&self) -> bool {
        self.flags & NetlinkFlags::NETLINK_F_KERNEL_SOCKET.bits() != 0
    }
    fn equals(&self, other: Arc<Mutex<Box<dyn NetlinkSocket>>>) -> bool {
        let binding = other.lock();
        let nlk = binding
            .deref()
            .as_any()
            .downcast_ref::<NetlinkSock>()
            .ok_or(SystemError::EINVAL)
            .clone()
            .unwrap();
        return self.portid == nlk.portid;
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
    fn flags(&self) -> Option<SockFlags> {
        Some(SockFlags::Dead)
    }
    fn sock_sndtimeo(&self, noblock: bool) -> i64 {
        if noblock {
            return 0;
        } else {
            return self.sk_sndtimeo;
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
impl NetlinkSocketWithCallback for NetlinkSock {
    fn sk_data_ready(&self, callback: impl Fn(i32) -> i32) { /* 实现 */
    }
}
impl NetlinkSock {
    /// 元数据的缓冲区的大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的接收缓冲区的大小 receive
    pub const DEFAULT_RX_BUF_SIZE: usize = 512 * 1024;
    /// 默认的发送缓冲区的大小 transmiss
    pub const DEFAULT_TX_BUF_SIZE: usize = 512 * 1024;
    pub fn new() -> NetlinkSock {
        let vec_of_vec_u8: Vec<Vec<u8>> = Vec::new();
        let mutex_protected = Mutex::new(vec_of_vec_u8);
        let data: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(mutex_protected);
        NetlinkSock {
            // sk: None,
            portid: 0,
            node: Arc::new(HListHead { first: None }),
            dst_portid: 0,
            dst_group: 0,
            flags: 0,
            subscriptions: 0,
            ngroups: 0,
            groups: vec![0; 32],
            bound: false,
            state: NetlinkState::NetlinkUnconnected,
            protocol: 1,
            max_recvmsg_len: 0,
            dump_done_errno: 0,
            cb_running: false,
            queue: Vec::new(),
            data,
            sk_sndtimeo: 0,
            sk_rcvtimeo: 0,
            callback: None,
        }
    }
    // https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#1078
    fn netlink_connect(&self, _endpoint: Endpoint) -> Result<(), SystemError> {
        Ok(())
    }

    // https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#1849
    /// 用户进程对netlink套接字调用 sendmsg() 系统调用后，内核执行netlink操作的总入口函数
    /// ## 参数
    /// - sock    - 指向用户进程的netlink套接字，也就是发送方的
    /// - msg     - 承载了发送方传递的netlink消息
    /// - len     - netlink消息长度
    /// ## 备注
    /// netlink套接字在创建的过程中(具体是在 netlink_create 开头)，已经和 netlink_ops (socket层netlink协议族的通用操作集合)关联,其中注册的 sendmsg 回调就是指向本函数
    fn netlink_send(&self, data: &[u8], address: Endpoint) -> Result<usize, SystemError> {
        log::info!("netlink_send: data: {:?}", data);
        // 一个有效的 Netlink 消息至少应该包含一个消息头
        if data.len() < size_of::<NLmsghdr>() {
            log::warn!("netlink_send: data too short, len: {}", data.len());
            return Err(SystemError::EINVAL);
        }
        #[allow(unsafe_code)]
        let header = unsafe { &*(data.as_ptr() as *const NLmsghdr) };
        if header.nlmsg_len > data.len() {
            log::warn!(
                "netlink_send: data too short, nlmsg_len: {}",
                header.nlmsg_len
            );
            return Err(SystemError::ENAVAIL);
        }
        // let message_type = NLmsgType::from(header.nlmsg_type);
        let mut buffer = self.data.lock();
        buffer.clear();

        let mut msg = Vec::new();
        let new_header = NLmsghdr {
            nlmsg_len: 0, // to be determined later
            nlmsg_type: NLmsgType::NLMSG_DONE,
            nlmsg_flags: NLmsgFlags::NLM_F_MULTI,
            nlmsg_seq: header.nlmsg_seq,
            nlmsg_pid: header.nlmsg_pid,
        };
        // 将新消息头序列化到 msg 中
        msg.push_ext(new_header);
        // 将消息体数据追加到 msg 中
        msg.extend_from_slice(data);
        // 确保 msg 的长度按照 4 字节对齐
        msg.align4();
        // msg 的开头设置消息长度。
        msg.set_ext(0, msg.len() as u32);
        // 将序列化后的 msg 添加到发送缓冲区 buffer 中
        buffer.push(msg);
        Ok(data.len())
    }

    // https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#1938
    /// 用户进程对 netlink 套接字调用 recvmsg() 系统调用后，内核执行 netlink 操作的总入口函数
    /// ## 参数
    /// - sock    - 指向用户进程的netlink套接字，也就是接收方的
    /// - msg     - 用于存放接收到的netlink消息
    /// - len     - 用户空间支持的netlink消息接收长度上限
    /// - flags   - 跟本次接收操作有关的标志位集合(主要来源于用户空间)
    fn netlink_recv(
        &self,
        msg: &mut [u8],
        flags: MessageFlag,
    ) -> Result<(usize, Endpoint), SystemError> {
        let mut copied: usize = 0;
        let mut buffer = self.data.lock();
        let msg_kernel = buffer.remove(0);

        // 判断是否是带外消息，如果是带外消息，直接返回错误码
        if flags == MessageFlag::OOB {
            log::warn!("netlink_recv: OOB message is not supported");
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        // 计算实际要复制的数据长度，不能超过 msg_from 的长度 或 msg 缓冲区的长度
        let actual_len = msg_kernel.len().min(msg.len());

        if !msg_kernel.is_empty() {
            msg[..actual_len].copy_from_slice(&msg_kernel[..actual_len]);
            copied = actual_len;
        } else {
            // 如果没有数据可复制，返回 0 字节被复制
            copied = 0;
        }

        let endpoint = Endpoint::Netlink(NetlinkEndpoint {
            addr: SockAddrNl {
                nl_family: AddressFamily::Netlink,
                nl_pad: 0,
                nl_pid: self.portid,
                nl_groups: 0,
            },
        });

        // 返回复制的字节数和端点信息
        log::debug!("netlink_recv: copied: {}, endpoint: {:?}", copied, endpoint);
        Ok((copied, endpoint))
    }
}

#[derive(Clone)]
pub struct Listeners {
    // todo: rcu
    // 动态位图，每一位代表一个组播组，如果对应位为 1，表示有监听
    masks: Vec<u64>,
}
impl Listeners {
    /// 创建一个新的 `Listeners` 实例，并将 `masks` 的所有位初始化为 0
    pub fn new() -> Listeners {
        let masks = vec![0u64; 32];
        Listeners { masks }
    }
}

fn initialize_netlink_table() -> RwLock<Vec<NetlinkTable>> {
    let mut tables = Vec::with_capacity(MAX_LINKS);
    for _ in 0..MAX_LINKS {
        tables.push(NetlinkTable::new());
    }
    log::info!("initialize_netlink_table,len:{}", tables.len());
    RwLock::new(tables)
}

lazy_static! {
    /// 一个维护全局的 NetlinkTable 向量，每一个元素代表一个 netlink 协议类型，最大数量为 MAX_LINKS
    pub static ref NL_TABLE: RwLock<Vec<NetlinkTable>> = initialize_netlink_table();
}
pub fn netlink_has_listeners(sk: &NetlinkSock, group: u32) -> i32 {
    log::info!("netlink_has_listeners");
    let mut res = 0;
    let protocol = sk.sk_protocol();

    // 获取读锁
    let nl_table = NL_TABLE.read();

    // 检查 protocol 是否在范围内
    if protocol >= nl_table.len() {
        log::error!(
            "Protocol {} is out of bounds, table's len is {}",
            protocol,
            nl_table.len()
        );
        return res;
    }

    // 获取对应的 NetlinkTable
    let netlink_table = &nl_table[protocol];

    // 检查 listeners 是否存在
    if let Some(listeners) = &netlink_table.listeners {
        // 检查 group 是否在范围内
        log::info!("listeners.masks:{:?}", listeners.masks);
        if group > 0 && (group as usize - 1) < listeners.masks.len() {
            res = listeners.masks[group as usize - 1] as i32;
        } else {
            log::error!(
                "Group {} is out of bounds, len is {}",
                group,
                listeners.masks.len()
            );
        }
    } else {
        log::error!("Listeners for protocol {} are None", protocol);
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
fn do_one_broadcast(
    sk: Arc<Mutex<Box<dyn NetlinkSocket>>>,
    info: &mut Box<NetlinkBroadcastData>,
) -> Result<(), SystemError> {
    log::info!("do_one_broadcast");
    // 从Arc<dyn NetlinkSocket>中获取NetlinkSock
    let nlk: Arc<NetlinkSock> = Arc::clone(&sk)
        .arc_any()
        .downcast()
        .map_err(|_| SystemError::EINVAL)?;
    // 如果源 sock 和目的 sock 是同一个则直接返回
    if info.exclude_sk.equals(sk.clone()) {
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
        if sk.lock().flags().is_some() & !NetlinkFlags::BROADCAST_SEND_ERROR.bits().is_zero() {
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
        if sk.lock().flags().is_some() & !NetlinkFlags::BROADCAST_SEND_ERROR.bits().is_zero() {
            info.delivery_failure = 1;
        }
    } else {
        info.congested |= ret;
        info.delivered = 1;
        info.skb_2 = Arc::new(RwLock::new(info.skb.read().clone()));
    }
    drop(sk);
    log::info!("do_one_broadcast success");
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
pub fn netlink_broadcast(
    ssk: &Arc<dyn NetlinkSocket>,
    skb: Arc<RwLock<SkBuff>>,
    portid: u32,
    group: u64,
    allocation: u32,
) -> Result<(), SystemError> {
    log::info!("netlink_broadcast");
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

    drop(info.skb);

    if info.delivery_failure != 0 {
        return Err(SystemError::ENOBUFS);
    }
    drop(info.skb_2);

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
fn netlink_broadcast_deliver(
    sk: Arc<Mutex<Box<dyn NetlinkSocket>>>,
    skb: &Arc<RwLock<SkBuff>>,
) -> i32 {
    log::info!("netlink_broadcast_deliver");
    let nlk: Arc<RwLock<NetlinkSock>> = Arc::clone(&sk)
        .arc_any()
        .downcast()
        .expect("Invalid downcast to LockedNetlinkSock");
    let nlk_guard = nlk.read();
    // 如果接收缓冲区的已分配内存小于或等于其总大小，并且套接字没有被标记为拥塞，则继续执行内部的代码块。
    if (sk.lock().sk_rmem_alloc() <= sk.lock().sk_rcvbuf())
        && !(nlk_guard.state == NetlinkState::NETLINK_S_CONGESTED)
    {
        // 如果满足接收条件，则设置skb的所有者是该netlink套接字
        netlink_skb_set_owner_r(skb, sk.clone());
        // 将 skb 发送到该 netlink 套接字，实际也就是将该 skb 放入了该套接字的接收队列中
        let _ = netlink_sendskb(sk.clone(), skb);
        // 如果套接字的接收缓冲区已经接收但尚未处理数据长度大于其接收缓冲的1/2，则返回1
        if sk.lock().sk_rmem_alloc() > (sk.lock().sk_rcvbuf() >> 1) {
            return 1;
        } else {
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
fn netlink_sendskb(sk: Arc<Mutex<Box<dyn NetlinkSocket>>>, skb: &Arc<RwLock<SkBuff>>) -> u32 {
    let len = skb.read().len;
    {
        // 将 skb 放入该 netlink 套接字接收队列末尾
        sk.lock().enqueue_skb(skb.clone());
        // 执行 sk_data_ready 回调通知该套接字有数据可读
        let nlk: Arc<NetlinkSock> = Arc::clone(&sk)
            .arc_any()
            .downcast()
            .expect("Invalid downcast to NetlinkSock");
        let _ = sk_data_ready(nlk);
    }
    return len;
}
// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#1337
/// 内核执行 netlink 单播消息
/// ## 参数
/// - ssk：源sock结构
/// - skb: 属于发送方的承载了 netlink 消息的 skb
/// - portid: 目的单播地址
/// - nonblock    - 1：非阻塞调用，2：阻塞调用
fn netlink_unicast(
    ssk: Arc<Mutex<Box<dyn NetlinkSocket>>>,
    skb: Arc<RwLock<SkBuff>>,
    portid: u32,
    nonblock: bool,
) -> Result<u32, SystemError> {
    let mut err: i32;
    // todo：重新调整skb的大小
    // skb = netlink_trim(skb, gfp_any());
    // 计算发送超时时间(如果是非阻塞调用，则返回 0)
    let timeo: i64 = ssk.lock().sock_sndtimeo(nonblock);
    loop {
        // 根据源sock结构和目的单播地址，得到目的sock结构
        let sk = netlink_getsockbyportid(ssk.clone(), portid);
        if sk.is_err() {
            drop(skb);
            return Err(sk.err().unwrap());
        }
        let sk = sk.unwrap();

        if sk.lock().is_kernel() {
            return Ok(netlink_unicast_kernel(sk, ssk, skb));
        }

        if sk_filter(&sk, &skb) {
            let err = skb.read().len;
            drop(skb);
            return Err(SystemError::EINVAL);
        }

        err = netlink_attachskb(sk.clone(), skb.clone(), timeo, ssk.clone()).unwrap() as i32;
        if err == 1 {
            continue; // 重试
        }
        if err != 0 {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        return Ok(netlink_sendskb(sk, &skb));
    }
}

// https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c#1316
/// 来自用户进程的 netlink 消息 单播 发往内核 netlink 套接字
/// ## 参数
/// - sk：目的sock结构
/// - skb：属于发送方的承载了netlink消息的skb
/// - ssk：源sock结构
/// ## 备注：
/// - skb的所有者在本函数中发生了变化
fn netlink_unicast_kernel(
    sk: Arc<Mutex<Box<dyn NetlinkSocket>>>,
    ssk: Arc<Mutex<Box<dyn NetlinkSocket>>>,
    skb: Arc<RwLock<SkBuff>>,
) -> u32 {
    let mut ret: u32;
    let nlk: Arc<RwLock<NetlinkSock>> = Arc::clone(&sk)
        .arc_any()
        .downcast()
        .map_err(|_| SystemError::EINVAL)
        .expect("Invalid downcast to LockedNetlinkSock");
    let nlk_guard = nlk.read();
    // ret = ECONNREFUSED = 111;
    ret = 111;
    // 检查内核netlink套接字是否注册了netlink_rcv回调(就是各个协议在创建内核netlink套接字时通常会传入的input函数)
    if nlk_guard.callback.is_some() {
        ret = skb.read().len;
        netlink_skb_set_owner_r(&skb, sk);
        // todo: netlink_deliver_tap_kernel(sk, ssk, skb);
        nlk_guard.callback.unwrap().netlink_rcv(skb.clone());
        drop(skb);
    } else {
        // 如果指定的内核netlink套接字没有注册netlink_rcv回调，就直接丢弃所有收到的netlink消息
        drop(skb);
    }
    return ret;
}
// https://code.dragonos.org.cn/s?refs=netlink_attachskb&project=linux-6.1.9
/// 将一个指定skb绑定到一个指定的属于用户进程的netlink套接字上
/// ## 参数
/// - sk: 目的套接字
/// - ssk: 源套接字
/// - skb: 待绑定的skb
/// - timeo: 超时时间
/// ## 返回值
/// - 小于0：表示错误，skb已经被释放，对套接字的引用也被释放。
/// - 0：表示继续执行，skb可以被附加到套接字上。
/// - 1：表示需要重新查找，可能因为等待超时或接收缓冲区不足。
fn netlink_attachskb(
    sk: Arc<Mutex<Box<dyn NetlinkSocket>>>,
    skb: Arc<RwLock<SkBuff>>,
    mut timeo: i64,
    ssk: Arc<Mutex<Box<dyn NetlinkSocket>>>,
) -> Result<u64, SystemError> {
    let nlk: Arc<RwLock<NetlinkSock>> = Arc::clone(&sk)
        .arc_any()
        .downcast()
        .map_err(|_| SystemError::EINVAL)?;
    let nlk_guard = nlk.read();
    let ssk_option: Option<Arc<Mutex<Box<dyn NetlinkSocket>>>> = Some(ssk.clone());

    /*
        如果目的netlink套接字上已经接收尚未处理的数据大小超过了接收缓冲区大小，
        或者目的netlink套接字被设置了拥挤标志，
        意味着该sbk不能立即被目的netlink套接字接收，需要加入等待队列
    */
    if sk.lock().sk_rmem_alloc() > sk.lock().sk_rcvbuf()
        || nlk_guard.state == NetlinkState::NETLINK_S_CONGESTED
    {
        // 申请一个等待队列
        let mut wq = WaitQueue::default();
        // 如果传入的超时时间为0, 意味着非阻塞调用，则丢弃这条 netlink 消息，并返回 EAGAIN
        if timeo == 0 {
            /* 如果该netlink消息对应的源sock结构不存在，或者该netlink消息来自kernel
             * 则对目的netlink套接字设置缓冲区溢出状态
             */
            if ssk_option.is_none() || ssk.lock().is_kernel() {
                netlink_overrun(&sk);
            }
            drop(skb);
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        // 程序运行到这里意味着是阻塞调用
        // 改变当前进程状态为可中断
        // __set_current_state(TASK_INTERRUPTIBLE);
        // todo: 将目的netlink套接字加入等待队列
        // add_wait_queue(&nlk_guard.wait, &wait);

        // 程序到这里意味着被唤醒了
        // 如果接收条件还是不满足，则要计算剩余的超时时间
        if (sk.lock().sk_rmem_alloc() > sk.lock().sk_rcvbuf() ||
        nlk_guard.state == NetlinkState::NETLINK_S_CONGESTED) &&
        // todo: sock_flag
		    sk.lock().flags() != Some(SockFlags::Dead)
        {
            timeo = schedule_timeout(timeo)?;
        }
        // 改变当前进程状态为运行
        // __set_current_state(TASK_RUNNING);
        // 将目的 netlink 套接字从等待队列中删除
        // remove_wait_queue(&nlk_guard.wait, &wait);

        // todo: 如果在等待期间接收到信号
        // if (signal_pending(current)) {
        // 	drop(skb);
        // 	return sock_intr_errno(*timeo);
        // }
        return Ok(1);
    }
    netlink_skb_set_owner_r(&skb, sk);
    return Ok(0);
}

fn netlink_getsockbyportid(
    ssk: Arc<Mutex<Box<dyn NetlinkSocket>>>,
    portid: u32,
) -> Result<Arc<Mutex<Box<dyn NetlinkSocket>>>, SystemError> {
    let sock: Arc<Mutex<Box<dyn NetlinkSocket>>> =
        netlink_lookup(ssk.lock().sk_protocol(), portid).unwrap();
    if Some(sock.clone()).is_none() {
        return Err(SystemError::ECONNREFUSED);
    }

    /* Don't bother queuing skb if kernel socket has no input function */
    let nlk_sock: Arc<RwLock<NetlinkSock>> = Arc::clone(&sock)
        .arc_any()
        .downcast()
        .map_err(|_| SystemError::EINVAL)?;
    let nlk_sock_guard = nlk_sock.read();
    let nlk_ssk: Arc<RwLock<NetlinkSock>> = Arc::clone(&ssk)
        .arc_any()
        .downcast()
        .map_err(|_| SystemError::EINVAL)?;
    let nlk_ssk_guard = nlk_ssk.read();
    /* dst_portid and sk_state can be changed in netlink_connect() */
    if sock.lock().sk_state() == NetlinkState::NetlinkUnconnected
        && (nlk_sock_guard.dst_portid) != nlk_ssk_guard.portid
    {
        return Err(SystemError::ECONNREFUSED);
    }
    return Ok(sock);
}

/// 设置 netlink 套接字的选项
fn netlink_setsockopt(
    nlk: &NetlinkSock,
    level: OptionsLevel,
    optname: usize,
    optval: &[u8],
) -> Result<(), SystemError> {
    if level != OptionsLevel::NETLINK {
        return Err(SystemError::ENOPROTOOPT);
    }
    let optlen = optval.len();
    let mut val: usize = 0;
    if optlen >= size_of::<usize>() {
        unsafe {
            if optval.len() >= size_of::<usize>() {
                // 将 optval 中的数据拷贝到 val 中
                copy_nonoverlapping(
                    optval.as_ptr(),
                    &mut val as *mut usize as *mut u8,
                    size_of::<usize>(),
                );
            } else {
                return Err(SystemError::EFAULT);
            }
        }
    } else {
        return Err(SystemError::EINVAL);
    }
    match optname {
        // add 和 drop 对应同一段代码
        NETLINK_ADD_MEMBERSHIP | NETLINK_DROP_MEMBERSHIP => {
            let group = val as u64;
            let mut nl_table = NL_TABLE.write();
            let netlink_table = &mut nl_table[nlk.protocol];
            let listeners = netlink_table.listeners.as_mut().unwrap();
            let group = group - 1;
            let mask = 1 << (group % 64);
            let idx = group / 64;
            if optname == NETLINK_ADD_MEMBERSHIP {
                listeners.masks[idx as usize] |= mask;
            } else {
                listeners.masks[idx as usize] &= !mask;
            }
        }
        NETLINK_PKTINFO => {
            //     if val != 0 {
            //     nlk.flags |= NetlinkFlags::RECV_PKTINFO.bits();
            //     } else {
            //     nlk.flags &= !NetlinkFlags::RECV_PKTINFO.bits();
            //     }
        }
        _ => {
            return Err(SystemError::ENOPROTOOPT);
        }
    }
    Ok(())
}

fn netlink_update_listeners(nlk: MutexGuard<NetlinkSock>) {
    log::info!("netlink_update_listeners");
    let mut nl_table = NL_TABLE.write();
    let netlink_table = &mut nl_table[nlk.protocol];
    let listeners = netlink_table.listeners.as_mut().unwrap();
    listeners.masks.clear();
    log::info!("nlk.ngroups:{}", nlk.ngroups);
    listeners.masks.resize(nlk.ngroups as usize, 0);
    log::info!("nlk.groups:{:?}", nlk.groups);
    for group in &nlk.groups {
        let mask = 1 << (group % 64);
        let idx = group / 64;

        listeners.masks[idx as usize] |= mask;
        log::info!(
            "group:{},mask:{},idx:{},masks:{:?}",
            group,
            mask,
            idx,
            listeners.masks
        );
    }
}

/// 重新分配 netlink 套接字的组
fn netlink_realloc_groups(nlk: &mut MutexGuard<NetlinkSock>) -> Result<(), SystemError> {
    let nl_table = NL_TABLE.write();
    let groups = nl_table[nlk.protocol].groups;
    if nl_table[nlk.protocol].registered == 0 {
        // 没有注册任何组
        log::warn!("netlink_realloc_groups: not registered");
        return Err(SystemError::ENOENT);
    }
    if nlk.ngroups >= groups {
        // 当前已分配的组数量 大于或等于 groups（当前协议的组数量），则没有必要重新分配\
        log::info!("netlink_realloc_groups: no need to realloc");
        return Ok(());
    }
    log::info!("nlk.ngroups:{},groups:{}", nlk.ngroups, groups);
    let mut new_groups = vec![0u32; groups as usize];
    log::info!("nlk.groups:{:?}", nlk.groups);
    // 当 nlk.ngroups 大于 0 时复制数据
    if nlk.ngroups > 0 {
        new_groups[..nlk.ngroups as usize].copy_from_slice(&nlk.groups);
    }
    nlk.groups = new_groups;
    nlk.ngroups = groups;
    log::info!("nlk.groups:{:?}", nlk.groups);
    Ok(())
}
