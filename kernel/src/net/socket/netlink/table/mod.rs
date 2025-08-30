mod multicast;

use crate::net::socket::netlink::addr::multicast::GroupIdSet;
use crate::net::socket::netlink::route::kernel::NetlinkRouteKernelSocket;
use crate::net::socket::netlink::route::message::RouteNlMessage;
use crate::net::socket::netlink::table::multicast::MulticastMessage;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessManager;
use crate::{libs::rand, net::socket::netlink::addr::NetlinkSocketAddr};
use crate::{
    libs::rwlock::RwLock,
    net::socket::netlink::{receiver::MessageReceiver, table::multicast::MulticastGroup},
};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::fmt::Debug;
use alloc::sync::Arc;
use core::any::Any;
use hashbrown::HashMap;
use system_error::SystemError;

pub const MAX_ALLOWED_PROTOCOL_ID: u32 = 32;
const MAX_GROUPS: u32 = 32;

#[derive(Debug)]
pub struct NetlinkSocketTable {
    route: Arc<RwLock<ProtocolSocketTable<RouteNlMessage>>>,
    // 在这里继续补充其他协议下的 socket table
    // 比如 uevent: Arc<RwLock<ProtocolSocketTable<UeventMessage>>>,
}

impl Default for NetlinkSocketTable {
    fn default() -> Self {
        Self {
            route: Arc::new(RwLock::new(ProtocolSocketTable::new())),
        }
    }
}

impl NetlinkSocketTable {
    pub fn route(&self) -> Arc<RwLock<ProtocolSocketTable<RouteNlMessage>>> {
        self.route.clone()
    }
}

#[derive(Debug)]
pub struct ProtocolSocketTable<Message: Debug> {
    unicast_sockets: BTreeMap<u32, MessageReceiver<Message>>,
    multicast_groups: Box<[MulticastGroup]>,
}

impl<Message: 'static + Debug> ProtocolSocketTable<Message> {
    fn new() -> Self {
        let multicast_groups = (0u32..MAX_GROUPS).map(|_| MulticastGroup::new()).collect();
        Self {
            unicast_sockets: BTreeMap::new(),
            multicast_groups,
        }
    }

    fn bind(
        &mut self,
        socket_table: Arc<RwLock<ProtocolSocketTable<Message>>>,
        addr: &NetlinkSocketAddr,
        receiver: MessageReceiver<Message>,
    ) -> Result<BoundHandle<Message>, SystemError> {
        let port = if addr.port() != 0 {
            addr.port()
        } else {
            let mut random_port = ProcessManager::current_pid().data() as u32;
            while random_port == 0 || self.unicast_sockets.contains_key(&random_port) {
                random_port = rand::soft_rand() as u32;
            }
            random_port
        };

        if self.unicast_sockets.contains_key(&port) {
            return Err(SystemError::EADDRINUSE);
        }

        self.unicast_sockets.insert(port, receiver);

        for group_id in addr.groups().ids_iter() {
            let group = &mut self.multicast_groups[group_id as usize];
            group.add_member(port);
        }

        Ok(BoundHandle::new(socket_table, port, addr.groups()))
    }

    fn unicast(&self, dst_port: u32, message: Message) -> Result<(), SystemError> {
        let Some(receiver) = self.unicast_sockets.get(&dst_port) else {
            return Ok(());
        };
        receiver.enqueue_message(message)
    }

    fn multicast(&self, dst_groups: GroupIdSet, message: Message) -> Result<(), SystemError>
    where
        Message: MulticastMessage,
    {
        for group_id in dst_groups.ids_iter() {
            let Some(group) = self.multicast_groups.get(group_id as usize) else {
                continue;
            };
            for member in group.members() {
                if let Some(receiver) = self.unicast_sockets.get(member) {
                    receiver.enqueue_message(message.clone())?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct BoundHandle<Message: 'static + Debug> {
    socket_table: Arc<RwLock<ProtocolSocketTable<Message>>>,
    port: u32,
    groups: GroupIdSet,
}

impl<Message: 'static + Debug> BoundHandle<Message> {
    fn new(
        socket_table: Arc<RwLock<ProtocolSocketTable<Message>>>,
        port: u32,
        groups: GroupIdSet,
    ) -> Self {
        Self {
            socket_table,
            port,
            groups,
        }
    }

    pub(super) const fn port(&self) -> u32 {
        self.port
    }

    pub(super) fn addr(&self) -> NetlinkSocketAddr {
        NetlinkSocketAddr::new(self.port, self.groups)
    }

    pub(super) fn add_groups(&mut self, groups: GroupIdSet) {
        let mut protocol_sockets = self.socket_table.write();

        for group_id in groups.ids_iter() {
            let group = &mut protocol_sockets.multicast_groups[group_id as usize];
            group.add_member(self.port);
        }

        self.groups.add_groups(groups);
    }

    pub(super) fn drop_groups(&mut self, groups: GroupIdSet) {
        let mut protocol_sockets = self.socket_table.write();

        for group_id in groups.ids_iter() {
            let group = &mut protocol_sockets.multicast_groups[group_id as usize];
            group.remove_member(self.port);
        }

        self.groups.drop_groups(groups);
    }

    pub(super) fn bind_groups(&mut self, groups: GroupIdSet) {
        let mut protocol_sockets = self.socket_table.write();

        for group_id in self.groups.ids_iter() {
            let group = &mut protocol_sockets.multicast_groups[group_id as usize];
            group.remove_member(self.port);
        }

        for group_id in groups.ids_iter() {
            let group = &mut protocol_sockets.multicast_groups[group_id as usize];
            group.add_member(self.port);
        }

        self.groups = groups;
    }
}

impl<Message: 'static + Debug> Drop for BoundHandle<Message> {
    fn drop(&mut self) {
        let mut protocol_sockets = self.socket_table.write();

        protocol_sockets.unicast_sockets.remove(&self.port);

        for group_id in self.groups.ids_iter() {
            let group = &mut protocol_sockets.multicast_groups[group_id as usize];
            group.remove_member(self.port);
        }
    }
}

pub trait SupportedNetlinkProtocol: Debug {
    type Message: 'static + Send + Debug;

    fn socket_table(netns: Arc<NetNamespace>) -> Arc<RwLock<ProtocolSocketTable<Self::Message>>>;

    fn bind(
        addr: &NetlinkSocketAddr,
        receiver: MessageReceiver<Self::Message>,
        netns: Arc<NetNamespace>,
    ) -> Result<BoundHandle<Self::Message>, SystemError> {
        let socket_table = Self::socket_table(netns);
        let mut socket_table_guard = socket_table.write();
        socket_table_guard.bind(socket_table.clone(), addr, receiver)
    }

    fn unicast(
        dst_port: u32,
        message: Self::Message,
        netns: Arc<NetNamespace>,
    ) -> Result<(), SystemError> {
        Self::socket_table(netns).read().unicast(dst_port, message)
    }

    //todo 多播消息用
    #[allow(unused)]
    fn multicast(
        dst_groups: GroupIdSet,
        message: Self::Message,
        netns: Arc<NetNamespace>,
    ) -> Result<(), SystemError>
    where
        Self::Message: MulticastMessage,
    {
        Self::socket_table(netns)
            .read()
            .multicast(dst_groups, message)
    }
}

#[derive(Debug)]
pub struct NetlinkRouteProtocol;

impl SupportedNetlinkProtocol for NetlinkRouteProtocol {
    type Message = RouteNlMessage;

    fn socket_table(netns: Arc<NetNamespace>) -> Arc<RwLock<ProtocolSocketTable<Self::Message>>> {
        netns.netlink_socket_table().route()
    }
}

pub fn is_valid_protocol(protocol: u32) -> bool {
    protocol < MAX_ALLOWED_PROTOCOL_ID
}

pub trait NetlinkKernelSocket: Debug + Send + Sync {
    fn protocol(&self) -> StandardNetlinkProtocol;

    /// 用于实现动态转换
    fn as_any_ref(&self) -> &dyn Any;
}

/// 为一个网络命名空间生成支持的 Netlink 内核套接字
pub fn generate_supported_netlink_kernel_sockets() -> HashMap<u32, Arc<dyn NetlinkKernelSocket>> {
    let mut sockets: HashMap<u32, Arc<dyn NetlinkKernelSocket>> =
        HashMap::with_capacity(MAX_ALLOWED_PROTOCOL_ID as usize);
    let route_socket = Arc::new(NetlinkRouteKernelSocket::new());
    sockets.insert(route_socket.protocol().into(), route_socket);

    // Add other supported netlink kernel sockets here
    sockets
}

#[expect(non_camel_case_types)]
#[repr(u32)]
#[derive(Debug, Clone, Copy)]
pub enum StandardNetlinkProtocol {
    /// Routing/device hook
    ROUTE = 0,
    /// Unused number
    UNUSED = 1,
    /// Reserved for user mode socket protocols
    USERSOCK = 2,
    /// Unused number, formerly ip_queue
    FIREWALL = 3,
    /// Socket monitoring
    SOCK_DIAG = 4,
    /// Netfilter/iptables ULOG
    NFLOG = 5,
    /// IPsec
    XFRM = 6,
    /// SELinux event notifications
    SELINUX = 7,
    /// Open-iSCSI
    ISCSI = 8,
    /// Auditing
    AUDIT = 9,
    FIB_LOOKUP = 10,
    CONNECTOR = 11,
    /// Netfilter subsystem
    NETFILTER = 12,
    IP6_FW = 13,
    /// DECnet routing messages
    DNRTMSG = 14,
    /// Kernel messages to userspace
    KOBJECT_UEVENT = 15,
    GENERIC = 16,
    /// Leave room for NETLINK_DM (DM Events)
    /// SCSI Transports
    SCSITRANSPORT = 18,
    ECRYPTFS = 19,
    RDMA = 20,
    /// Crypto layer
    CRYPTO = 21,
    /// SMC monitoring
    SMC = 22,
}

impl From<StandardNetlinkProtocol> for u32 {
    fn from(value: StandardNetlinkProtocol) -> Self {
        value as u32
    }
}

impl TryFrom<u32> for StandardNetlinkProtocol {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(StandardNetlinkProtocol::ROUTE),
            1 => Ok(StandardNetlinkProtocol::UNUSED),
            2 => Ok(StandardNetlinkProtocol::USERSOCK),
            3 => Ok(StandardNetlinkProtocol::FIREWALL),
            4 => Ok(StandardNetlinkProtocol::SOCK_DIAG),
            5 => Ok(StandardNetlinkProtocol::NFLOG),
            6 => Ok(StandardNetlinkProtocol::XFRM),
            7 => Ok(StandardNetlinkProtocol::SELINUX),
            8 => Ok(StandardNetlinkProtocol::ISCSI),
            9 => Ok(StandardNetlinkProtocol::AUDIT),
            10 => Ok(StandardNetlinkProtocol::FIB_LOOKUP),
            11 => Ok(StandardNetlinkProtocol::CONNECTOR),
            12 => Ok(StandardNetlinkProtocol::NETFILTER),
            13 => Ok(StandardNetlinkProtocol::IP6_FW),
            14 => Ok(StandardNetlinkProtocol::DNRTMSG),
            15 => Ok(StandardNetlinkProtocol::KOBJECT_UEVENT),
            16 => Ok(StandardNetlinkProtocol::GENERIC),
            18 => Ok(StandardNetlinkProtocol::SCSITRANSPORT),
            19 => Ok(StandardNetlinkProtocol::ECRYPTFS),
            20 => Ok(StandardNetlinkProtocol::RDMA),
            21 => Ok(StandardNetlinkProtocol::CRYPTO),
            22 => Ok(StandardNetlinkProtocol::SMC),
            _ => Err(()),
        }
    }
}
