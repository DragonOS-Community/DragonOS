//参考https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c

// netlink_proto结构体
// static mut NETLINK_PROTO: netlink_proto::NetlinkProto = netlink_proto::NetlinkProto {
//     name: String::from("NETLINK"),
//     owner: std::sync::Arc::downgrade(THIS_MODULE),
//     obj_size: std::mem::size_of::<netlink_sock>(),
// };

// SPDX-License-Identifier: GPL-2.0

use core::{any::Any, fmt::Debug, hash::Hash, ops::Deref};

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
// NetlinkTable how to design? a struct or use NetlinkTableEntry as struct?
pub struct NetlinkTable {
    listeners: Option<listeners>,
    registed: u32,
    flags: u32,
    groups: u32,
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
pub struct sock{
    sk_protocol: u16,
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
    fn sk_protocol(&self) -> u16;
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
    fn sk_protocol(&self) -> u16{
        self.sk_protocol
    }
    fn is_kernel(&self) -> bool{
        true
    }
}

trait NetlinkSock:{
    fn portid(&self) -> u32;
    fn dst_portid(&self) -> u32;
    fn dst_group(&self) -> u32;
    fn flags(&self) -> u32;
    fn subscriptions(&self) -> u32;
    fn ngroups(&self) -> u32;
    fn groups(&self) -> &[u64];
    fn state(&self) -> u64;
    fn max_recvmsg_len(&self) -> usize;
    // fn wait(&self) -> &wait_queue_head_t;
    fn bound(&self) -> bool;
    fn cb_running(&self) -> bool;
    fn dump_done_errno(&self) -> i32;
    // fn cb(&self) -> &struct_netlink_callback;
    // fn cb_mutex(&self) -> &struct_mutex;
    // fn cb_def_mutex(&self) -> &struct_mutex;
    // fn netlink_rcv(&self, skb: &struct_sk_buff) -> void;
    // fn netlink_bind(&self, net: &struct_net, group: int) -> int;
    // fn netlink_unbind(&self, net: &struct_net, group: int) -> void;
    // fn module(&self) -> &struct_module;
    // fn node(&self) -> &struct_rhash_head;
    // fn rcu(&self) -> &struct_rcu_head;
    // fn work(&self) -> &struct_work_struct;
}
/* linux：struct sock has to be the first member of netlink_sock */
impl dyn NetlinkSock {
   
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
    if let Some(listeners) = &nl_table[sk.sk_protocol as usize].listeners {
        if group - 1 < nl_table[sk.sk_protocol as usize].groups {
            res = listeners.masks[group as usize - 1]as i32;
        }
    }
    res
}