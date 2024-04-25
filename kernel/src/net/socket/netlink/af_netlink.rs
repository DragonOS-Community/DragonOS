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

// Flags constants
const NETLINK_F_KERNEL_SOCKET: u32 = 0x1;
const NETLINK_F_RECV_PKTINFO: u32 = 0x2;
const NETLINK_F_BROADCAST_SEND_ERROR: u32 = 0x4;
const NETLINK_F_RECV_NO_ENOBUFS: u32 = 0x8;
const NETLINK_F_LISTEN_ALL_NSID: u32 = 0x10;
const NETLINK_F_CAP_ACK: u32 = 0x20;
const NETLINK_F_EXT_ACK: u32 = 0x40;
const NETLINK_F_STRICT_CHK: u32 = 0x80;


pub struct sockaddr_nl {
    pub nl_family: SA_FAMILY_T,
    pub nl_pad: u16,
    pub nl_pid: u32,
    pub nl_groups: u32,
}
//需要大改
struct NetlinkSocket {
    sk: sockaddr_nl,
    portid: u32,
    dst_portid: u32,
    dst_group: u32,
    flags: u32,
    subscriptions: u32,
    ngroups: u32,
    groups: Vec<u32>,
    //struct
    state: u32,
    max_recvmsg_len: usize,
    //no std
    wait: crossbeam_utils::atomic::AtomicCell<std::sync::Mutex<std::wait::WakeUp>>,
    bound: bool,
    cb_running: bool,
    dump_done_errno: i32,
    cb: Option<Box<dyn FnMut(libc::sockaddr_nl, &[u8])>>,
    //todo
    cb_mutex: Mutex<()>,
    cb_def_mutex: Mutex<()>,
    netlink_rcv: Box<dyn FnMut(Vec<u8>)>,
    // trait method
    netlink_bind: Option<Box<dyn FnMut(libc::sockaddr_nl, u32) -> bool>>,
    netlink_unbind: Option<Box<dyn FnMut(libc::sockaddr_nl, u32)>>,
    module: Option<std::ffi::c_void>,
    node: crossbeam_utils::atomic::AtomicCell<libc::rhashtable_node>,
    // no libc
    //rcu: libc::rcu_head,
    work: workqueue::Work,
}
// NetlinkTable how to design? a struct or use NetlinkTableEntry as struct?
struct NetlinkTable {
    hash: rhash_table,
    mc_list: std::collections::HashList<libc::sockaddr_nl>,
    listeners: RCuListeners,
    flags: u32,
    groups: u32,
    cb_mutex: Mutex<()>,
    module: Option<std::ffi::c_void>,
    bind: Option<Box<dyn FnMut(libc::sockaddr_nl, u32) -> bool>>,
    unbind: Option<Box<dyn FnMut(libc::sockaddr_nl, u32)>>,
    compare: Option<Box<dyn FnMut(libc::sockaddr_nl, libc::sockaddr_nl) -> bool>>,
    registered: bool,
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
    #[cfg(all(feature = "CONFIG_BPF_SYSCALL", feature = "CONFIG_PROC_FS"))]
    {
        // 注册BPF迭代器
        // ...
    }
    
    
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