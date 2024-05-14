//参考https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/af_netlink.c
use std::alloc::{alloc, Layout};
use std::error::Error;
use std::sync::{Mutex, Arc};
use std::{error, fmt, result};

// netlink_proto结构体
static mut NETLINK_PROTO: netlink_proto::NetlinkProto = netlink_proto::NetlinkProto {
    name: String::from("NETLINK"),
    owner: std::sync::Arc::downgrade(THIS_MODULE),
    obj_size: std::mem::size_of::<netlink_sock>(),
};

// SPDX-License-Identifier: GPL-2.0


use crossbeam_utils::atomic::AtomicCell;
use crate::net::sock::sock;

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
    }
}

pub struct sockaddr_nl {
    pub nl_family: SA_FAMILY_T,
    pub nl_pad: u16,
    pub nl_pid: u32,
    pub nl_groups: u32,
}

trait NetlinkSocket {
    fn send(&self, msg: &[u8]) -> Result<(), Box<dyn Error>>;
    fn recv(&self) -> Result<Vec<u8>, Box<dyn Error>>;
    fn bind(&self, addr: libc::sockaddr_nl) -> Result<(), Box<dyn Error>>;
    fn unbind(&self, addr: libc::sockaddr_nl) -> Result<(), Box<dyn Error>>;
    fn register(&self, listener: Box<dyn NetlinkMessageHandler>);
    fn unregister(&self, listener: Box<dyn NetlinkMessageHandler>);

}
// NetlinkTable how to design? a struct or use NetlinkTableEntry as struct?
trait NetlinkTable {
    fn hash(&self)->RhashTable;
    fn mc_list(&self)->hlist_head;
    fn listeners(&self)->RCuListeners;
    fn flags(&self)->u32;
    fn groups(&self)->u32;
    fn cb_mutex(&self)->Mutex;
    fn module(&self)->Module;
    fn bind(net:Net, group: u32);
    fn unbind(net:Net, group: u32);
    fn compare(net:Net, sock:sock);
    fn registed(&self)->u32;
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

fn netlink_proto_init() -> Result<(), Box<dyn Error>> {

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
        eprintln!("Failed to initialize netlink protocol: {:?}", e);
        std::process::exit(1);
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
trait sock{
    fn sk(&self) -> &sock;
    fn sk_prot(&self) -> &proto;
    fn sk_family(&self) -> i32;
    fn sk_state(&self) -> i32;
}

trait NetlinkSockTrait: sock {
    fn portid(&self) -> u32;
    fn dst_portid(&self) -> u32;
    fn dst_group(&self) -> u32;
    fn flags(&self) -> u32;
    fn is_kernel(&self) -> bool;
    fn subscriptions(&self) -> u32;
    fn ngroups(&self) -> u32;
    fn groups(&self) -> &[u64];
    fn state(&self) -> u64;
    fn max_recvmsg_len(&self) -> size_t;
    fn wait(&self) -> &wait_queue_head_t;
    fn bound(&self) -> bool;
    fn cb_running(&self) -> bool;
    fn dump_done_errno(&self) -> int;
    fn cb(&self) -> &struct_netlink_callback;
    fn cb_mutex(&self) -> &struct_mutex;
    fn cb_def_mutex(&self) -> &struct_mutex;
    fn netlink_rcv(&self, skb: &struct_sk_buff) -> void;
    fn netlink_bind(&self, net: &struct_net, group: int) -> int;
    fn netlink_unbind(&self, net: &struct_net, group: int) -> void;
    fn module(&self) -> &struct_module;
    fn node(&self) -> &struct_rhash_head;
    fn rcu(&self) -> &struct_rcu_head;
    fn work(&self) -> &struct_work_struct;
}
/* linux：struct sock has to be the first member of netlink_sock */
impl NetlinkSockTrait for netlink_sock {
    fn sk(&self) -> &sock {
        &self.sk
    }
    fn portid(&self) -> u32 {
        self.portid
    }
    fn dst_portid(&self) -> u32 {
        self.dst_portid
    }
    fn dst_group(&self) -> u32 {
        self.dst_group
    }
    fn flags(&self) -> u32 {
        self.flags
    }
    fn is_kernel(&self) -> bool {
        self.flags & NETLINK_F_KERNEL_SOCKET != 0
    }
    fn subscriptions(&self) -> u32 {
        self.subscriptions
    }
    fn ngroups(&self) -> u32 {
        self.ngroups
    }
    fn groups(&self) -> &[u64] {
        &self.groups
    }
    fn state(&self) -> u64 {
        self.state
    }
    fn max_recvmsg_len(&self) -> size_t {
        self.max_recvmsg_len
    }
    fn wait(&self) -> &wait_queue_head_t {
        &self.wait
    }
    fn bound(&self) -> bool {
        self.bound
    }
    fn cb_running(&self) -> bool {
        self.cb_running
    }
    fn dump_done_errno(&self) -> int {
        self.dump_done_errno
    }
    fn cb(&self) -> &struct_netlink_callback {
        &self.cb
    }
    fn cb_mutex(&self) -> &struct_mutex {
        &self.cb_mutex
    }
    fn cb_def_mutex(&self) -> &struct_mutex {
        &self.cb_def_mutex
    }
    fn netlink_rcv(&self, skb: &struct_sk_buff) -> void {
        self.netlink_rcv(skb)
    }
    fn netlink_bind(&self, net: &struct_net, group: int) -> int {
        self.netlink_bind(net, group)
    }
    fn netlink_unbind(&self, net: &struct_net, group: int) -> void {
        self.netlink_unbind(net, group)
    }
    fn module(&self) -> &struct_module {
        &self.module
    }
    fn node(&self) -> &struct_rhash_head {
        &self.node
    }
    fn rcu(&self) -> &struct_rcu_head {
        &self.rcu
    }
    fn work(&self) -> &struct_work_struct {
        &self.work
    }
}
fn netlink_create(net: &Net, socket: &mut Socket, protocol: i32, _kern: bool) -> Result<(), Error> {
    let mut module: Option<Box<Module>> = None;
    let mut cb_mutex: Mutex;
    let mut nlk = NetlinkSock::new();
    let mut bind: Option<fn(&Net, usize)> = None;
    let mut unbind: Option<fn(&Net, usize)> = None;

    // 假设我们有一个类型来跟踪协议最大值
    const MAX_LINKS: i32 = 1024;

    if socket.type_ != SocketType::Raw && socket.type_ != SocketType::Dgram {
        return Err(Error::SocketTypeNotSupported);
    }

    if protocol < 0 || protocol >= MAX_LINKS {
        return Err(Error::ProtocolNotSupported);
    }

    // 安全的数组索引封装
    let protocol = protocol as usize;

    // 这里简化了锁和模块加载逻辑

    // 假设成功加载了模块和相关函数
    module = Some(Box::new(Module));
    bind = Some(bind_function);
    unbind = Some(unbind_function);

    // 继续其他的逻辑
    // ...

    Ok(())
}

// 假设的绑定和解绑函数
fn bind_function(net: &Net, group: usize) {
    // 实现细节
}

fn unbind_function(net: &Net, group: usize) {
    // 实现细节
}

trait callback_head {
    fn next(&self) -> Option<&callback_head>;
    fn func(callback_head: &callback_head);
}

struct listeners {
    // Recursive Wakeup Unlocking?
	rcu: callback_head,
	masks:Vec<u64>,
}

fn netlink_has_listeners(sk: &sock, group: u64) -> i32 {
    let mut res = 0;
    let listeners:listeners= listeners::new();
    // 判断是否是内核socket
    assert!(sk.is_kernel(), "sk is not a kernel socket");
    let my_data = Mutex::new(my_shared_data);
    // 读锁保护下读取nl_table[sk->sk_protocol].listeners

    res
    
}