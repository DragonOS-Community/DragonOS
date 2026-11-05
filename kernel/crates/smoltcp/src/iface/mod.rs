/*! Network interface logic.

The `iface` module deals with the *network interfaces*. It filters incoming frames,
provides lookup and caching of hardware addresses, and handles management packets.
*/

mod fragmentation;
#[cfg(feature = "alloc")]
mod ingress_packet;
mod interface;
#[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
mod neighbor;
mod route;
#[cfg(feature = "proto-rpl")]
mod rpl;
mod socket_meta;
mod socket_set;

pub mod packet;

#[cfg(feature = "alloc")]
pub use self::ingress_packet::{
    IngressPacket, IngressPacketAllocError, IpIngressFilter, LocalInputVerdict, PreRoutingVerdict,
    RouteInputVerdict, RoutedIngressPacket,
};

#[cfg(feature = "multicast")]
pub use self::interface::multicast::MulticastError;
pub use self::interface::{
    Config, Interface, InterfaceInner as Context, IpMtuError, IpPacketDispatchError,
    Ipv4PacketDispatchError, PollIngressSingleResult, PollResult,
};

pub use self::route::{Route, RouteTableFull, Routes};
#[cfg(feature = "alloc")]
pub use self::socket_set::SocketSetStorage;
pub use self::socket_set::{SocketHandle, SocketSet, SocketStorage};
#[cfg(all(feature = "alloc", feature = "socket-tcp"))]
pub use self::socket_set::{TcpIngressHandler, TcpIngressResult, TcpListenRegistry};
#[cfg(all(feature = "alloc", feature = "socket-udp"))]
pub use self::socket_set::{UdpIngressHandler, UdpIngressResult};
