// Heads up! Before working on this file you should read the parts
// of RFC 1122 that discuss Ethernet, ARP and IP for any IPv4 work
// and RFCs 8200 and 4861 for any IPv6 and NDISC work.

#[cfg(test)]
mod tests;

#[cfg(feature = "medium-ethernet")]
mod ethernet;
#[cfg(feature = "medium-ieee802154")]
mod ieee802154;

#[cfg(feature = "proto-ipv4")]
mod ipv4;
#[cfg(feature = "proto-ipv6")]
mod ipv6;
#[cfg(feature = "proto-sixlowpan")]
mod sixlowpan;

#[cfg(feature = "multicast")]
pub mod multicast;
#[cfg(feature = "socket-tcp")]
mod tcp;
#[cfg(any(feature = "socket-udp", feature = "socket-dns"))]
mod udp;

#[cfg(feature = "alloc")]
use super::ingress_packet::{
    IngressPacket, IpIngressFilter, LocalInputVerdict, PreRoutingVerdict, RouteInputVerdict,
    RoutedIngressPacket,
};
use super::packet::*;

use core::result::Result;
use heapless::Vec;

#[cfg(feature = "_proto-fragmentation")]
use super::fragmentation::FragKey;
#[cfg(feature = "proto-ipv4-fragmentation")]
use super::fragmentation::FragmentRange;
#[cfg(any(feature = "proto-ipv4", feature = "proto-sixlowpan"))]
use super::fragmentation::PacketAssemblerSet;
use super::fragmentation::{Fragmenter, FragmentsBuffer};

#[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
use super::neighbor::{Answer as NeighborAnswer, Cache as NeighborCache};
use super::socket_set::SocketSet;
use crate::config::{IFACE_MAX_ADDR_COUNT, IFACE_MAX_SIXLOWPAN_ADDRESS_CONTEXT_COUNT};
use crate::iface::Routes;
use crate::phy::PacketMeta;
use crate::phy::{
    ChecksumCapabilities, Device, DeviceCapabilities, IpOutputClass, IpOutputError, Medium,
    RxToken, TxToken,
};
use crate::rand::Rand;
use crate::socket::*;
use crate::time::{Duration, Instant};

use crate::wire::*;

macro_rules! check {
    ($e:expr) => {
        match $e {
            Ok(x) => x,
            Err(_) => {
                // concat!/stringify! doesn't work with defmt macros
                #[cfg(not(feature = "defmt"))]
                net_trace!(concat!("iface: malformed ", stringify!($e)));
                #[cfg(feature = "defmt")]
                net_trace!("iface: malformed");
                return Default::default();
            }
        }
    };
}
use check;

/// Result returned by [`Interface::poll`].
///
/// This contains information on whether socket states might have changed.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PollResult {
    /// Socket state is guaranteed to not have changed.
    None,
    /// You should check the state of sockets again for received data or completion of operations.
    SocketStateChanged,
}

/// Result returned by [`Interface::poll_ingress_single`].
///
/// This contains information on whether a packet was processed or not,
/// and whether it might've affected socket states.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PollIngressSingleResult {
    /// No packet was processed. You don't need to call [`Interface::poll_ingress_single`]
    /// again, until more packets arrive.
    ///
    /// Socket state is guaranteed to not have changed.
    None,
    /// A packet was processed.
    ///
    /// There may be more packets in the device's RX queue, so you should call [`Interface::poll_ingress_single`] again.
    ///
    /// Socket state is guaranteed to not have changed.
    PacketProcessed,
    /// A packet was processed, which might have caused socket state to change.
    ///
    /// There may be more packets in the device's RX queue, so you should call [`Interface::poll_ingress_single`] again.
    ///
    /// You should check the state of sockets again for received data or completion of operations.
    SocketStateChanged,
}

/// Error returned by [`Interface::dispatch_ip_packet`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum IpPacketDispatchError {
    /// The supplied packet is incomplete or does not match the next-hop family.
    Malformed,
    /// A permanent neighbor address does not match the interface medium.
    InvalidHardwareAddress,
    /// The interface cannot select a source address for neighbor discovery.
    NoRoute,
    /// The selected transmit backend has no capacity; retry after it becomes
    /// writable again.
    Exhausted,
    /// Neighbor discovery is in progress; retry no earlier than `retry_at`.
    NeighborPending { retry_at: Instant },
}

/// Compatibility name for the original IPv4-only dispatch API.
pub type Ipv4PacketDispatchError = IpPacketDispatchError;

/// Error returned when an IP MTU cannot be used by the interface.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum IpMtuError {
    /// The IP MTU is below the minimum supported by the stack.
    TooSmall,
    /// The requested MTU exceeds the device capability captured at creation.
    TooLarge,
    /// Adding the link-layer header to the IP MTU overflowed `usize`.
    FrameSizeOverflow,
}

// IPv4 requires hosts to accept datagrams of at least 68 bytes. This also keeps
// every currently supported transport/header calculation non-negative (the
// largest one is DHCPv4's 60-byte IPv4 header plus its 8-byte UDP header).
const MIN_IP_MTU: usize = 68;

/// A  network interface.
///
/// The network interface logically owns a number of other data structures; to avoid
/// a dependency on heap allocation, it instead owns a `BorrowMut<[T]>`, which can be
/// a `&mut [T]`, or `Vec<T>` if a heap is available.
pub struct Interface {
    pub inner: InterfaceInner,
    fragments: FragmentsBuffer,
    fragmenter: Fragmenter,
    #[cfg(feature = "alloc")]
    ingress_scratch: crate::storage::PacketBytes,
}

/// The device independent part of an Ethernet network interface.
///
/// Separating the device from the data required for processing and dispatching makes
/// it possible to borrow them independently. For example, the tx and rx tokens borrow
/// the `device` mutably until they're used, which makes it impossible to call other
/// methods on the `Interface` in this time (since its `device` field is borrowed
/// exclusively). However, it is still possible to call methods on its `inner` field.
pub struct InterfaceInner {
    pub caps: DeviceCapabilities,
    max_frame_mtu: usize,
    pub now: Instant,
    rand: Rand,

    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    neighbor_cache: NeighborCache,
    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    neighbor_discovery_enabled: bool,
    hardware_addr: HardwareAddress,
    is_loopback: bool,
    #[cfg(feature = "medium-ieee802154")]
    sequence_no: u8,
    #[cfg(feature = "medium-ieee802154")]
    pan_id: Option<Ieee802154Pan>,
    #[cfg(feature = "proto-ipv4-fragmentation")]
    ipv4_id: u16,
    #[cfg(feature = "proto-sixlowpan")]
    sixlowpan_address_context:
        Vec<SixlowpanAddressContext, IFACE_MAX_SIXLOWPAN_ADDRESS_CONTEXT_COUNT>,
    #[cfg(feature = "proto-sixlowpan-fragmentation")]
    tag: u16,
    ip_addrs: Vec<IpCidr, IFACE_MAX_ADDR_COUNT>,
    any_ip: bool,
    /// Whether connected prefixes are represented explicitly in `routes`.
    route_table_includes_connected_prefixes: bool,
    routes: Routes,
    #[cfg(feature = "multicast")]
    multicast: multicast::State,
}

/// Configuration structure used for creating a network interface.
#[non_exhaustive]
pub struct Config {
    /// Random seed.
    ///
    /// It is strongly recommended that the random seed is different on each boot,
    /// to avoid problems with TCP port/sequence collisions.
    ///
    /// The seed doesn't have to be cryptographically secure.
    pub random_seed: u64,

    /// Set the Hardware address the interface will use.
    ///
    /// # Panics
    /// Creating the interface panics if the address is not unicast.
    pub hardware_addr: HardwareAddress,

    /// Whether this interface is the loopback device (as opposed to a TUN
    /// device, which also uses `Medium::Ip`).
    pub is_loopback: bool,

    /// Set the IEEE802.15.4 PAN ID the interface will use.
    ///
    /// **NOTE**: we use the same PAN ID for destination and source.
    #[cfg(feature = "medium-ieee802154")]
    pub pan_id: Option<Ieee802154Pan>,
}

impl Config {
    pub fn new(hardware_addr: HardwareAddress) -> Self {
        Config {
            random_seed: 0,
            hardware_addr,
            is_loopback: false,
            #[cfg(feature = "medium-ieee802154")]
            pan_id: None,
        }
    }
}

impl Interface {
    /// Create a network interface using the previously provided configuration.
    ///
    /// # Panics
    /// This function panics if the [`Config::hardware_address`] does not match
    /// the medium of the device.
    pub fn new(config: Config, device: &mut (impl Device + ?Sized), now: Instant) -> Self {
        let caps = device.capabilities();
        let max_frame_mtu = caps.max_transmission_unit;
        assert_eq!(
            config.hardware_addr.medium(),
            caps.medium,
            "The hardware address does not match the medium of the interface."
        );

        let mut rand = Rand::new(config.random_seed);

        #[cfg(feature = "medium-ieee802154")]
        let mut sequence_no;
        #[cfg(feature = "medium-ieee802154")]
        loop {
            sequence_no = (rand.rand_u32() & 0xff) as u8;
            if sequence_no != 0 {
                break;
            }
        }

        #[cfg(feature = "proto-sixlowpan")]
        let mut tag;

        #[cfg(feature = "proto-sixlowpan")]
        loop {
            tag = rand.rand_u16();
            if tag != 0 {
                break;
            }
        }

        #[cfg(feature = "proto-ipv4")]
        let mut ipv4_id;

        #[cfg(feature = "proto-ipv4")]
        loop {
            ipv4_id = rand.rand_u16();
            if ipv4_id != 0 {
                break;
            }
        }

        Interface {
            fragments: FragmentsBuffer {
                #[cfg(feature = "proto-sixlowpan")]
                decompress_buf: [0u8; sixlowpan::MAX_DECOMPRESSED_LEN],

                #[cfg(feature = "_proto-fragmentation")]
                assembler: PacketAssemblerSet::new(),
                #[cfg(feature = "_proto-fragmentation")]
                reassembly_timeout: Duration::from_secs(60),
            },
            fragmenter: Fragmenter::new(),
            #[cfg(feature = "alloc")]
            ingress_scratch: crate::storage::PacketBytes::new(),
            inner: InterfaceInner {
                now,
                caps,
                max_frame_mtu,
                hardware_addr: config.hardware_addr,
                is_loopback: config.is_loopback,
                ip_addrs: Vec::new(),
                any_ip: false,
                route_table_includes_connected_prefixes: false,
                routes: Routes::new(),
                #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
                neighbor_cache: NeighborCache::new(),
                #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
                neighbor_discovery_enabled: true,
                #[cfg(feature = "multicast")]
                multicast: multicast::State::new(),
                #[cfg(feature = "medium-ieee802154")]
                sequence_no,
                #[cfg(feature = "medium-ieee802154")]
                pan_id: config.pan_id,
                #[cfg(feature = "proto-sixlowpan-fragmentation")]
                tag,
                #[cfg(feature = "proto-ipv4-fragmentation")]
                ipv4_id,
                #[cfg(feature = "proto-sixlowpan")]
                sixlowpan_address_context: Vec::new(),
                rand,
            },
        }
    }

    /// Get the socket context.
    ///
    /// The context is needed for some socket methods.
    pub fn context(&mut self) -> &mut InterfaceInner {
        &mut self.inner
    }

    /// Get the HardwareAddress address of the interface.
    ///
    /// # Panics
    /// This function panics if the medium is not Ethernet or Ieee802154.
    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    pub fn hardware_addr(&self) -> HardwareAddress {
        #[cfg(all(feature = "medium-ethernet", not(feature = "medium-ieee802154")))]
        assert!(self.inner.caps.medium == Medium::Ethernet);
        #[cfg(all(feature = "medium-ieee802154", not(feature = "medium-ethernet")))]
        assert!(self.inner.caps.medium == Medium::Ieee802154);

        #[cfg(all(feature = "medium-ieee802154", feature = "medium-ethernet"))]
        assert!(
            self.inner.caps.medium == Medium::Ethernet
                || self.inner.caps.medium == Medium::Ieee802154
        );

        self.inner.hardware_addr
    }

    /// Set the HardwareAddress address of the interface.
    ///
    /// # Panics
    /// This function panics if the address is not unicast, and if the medium is not Ethernet or
    /// Ieee802154.
    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    pub fn set_hardware_addr(&mut self, addr: HardwareAddress) {
        #[cfg(all(feature = "medium-ethernet", not(feature = "medium-ieee802154")))]
        assert!(self.inner.caps.medium == Medium::Ethernet);
        #[cfg(all(feature = "medium-ieee802154", not(feature = "medium-ethernet")))]
        assert!(self.inner.caps.medium == Medium::Ieee802154);

        #[cfg(all(feature = "medium-ieee802154", feature = "medium-ethernet"))]
        assert!(
            self.inner.caps.medium == Medium::Ethernet
                || self.inner.caps.medium == Medium::Ieee802154
        );

        InterfaceInner::check_hardware_addr(&addr);
        self.inner.hardware_addr = addr;
    }

    /// Set the IP-layer maximum transmission unit used by this interface.
    ///
    /// The cached device capabilities are updated immediately, so socket MSS
    /// calculation and packet fragmentation observe the new value without
    /// reconstructing the interface. A pending native IPv4 fragmentation
    /// sequence re-evaluates this value for every continuation fragment.
    /// Route-selected egress overrides intentionally retain their own MTU.
    pub fn set_ip_mtu(&mut self, ip_mtu: usize) -> Result<(), IpMtuError> {
        if ip_mtu < MIN_IP_MTU {
            return Err(IpMtuError::TooSmall);
        }

        let frame_mtu = match self.inner.caps.medium {
            #[cfg(feature = "medium-ethernet")]
            Medium::Ethernet => ip_mtu
                .checked_add(EthernetFrame::<&[u8]>::header_len())
                .ok_or(IpMtuError::FrameSizeOverflow)?,
            #[cfg(feature = "medium-ip")]
            Medium::Ip => ip_mtu,
            #[cfg(feature = "medium-ieee802154")]
            Medium::Ieee802154 => ip_mtu,
        };
        if frame_mtu > self.inner.max_frame_mtu {
            return Err(IpMtuError::TooLarge);
        }
        self.inner.caps.max_transmission_unit = frame_mtu;
        Ok(())
    }

    /// Return whether dynamic link-layer neighbor discovery is enabled.
    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    pub fn neighbor_discovery_enabled(&self) -> bool {
        self.inner.neighbor_discovery_enabled
    }

    /// Enable or disable dynamic ARP/NDISC neighbor discovery.
    ///
    /// Disabling discovery drops dynamically learned mappings and rate-limit
    /// state. Unknown unicast neighbors then use the interface hardware address
    /// directly, matching the Linux `IFF_NOARP` neighbor initialization model,
    /// without transmitting discovery packets. Explicit hardware addresses
    /// supplied by an integration remain authoritative.
    /// Changing modes drops any pending fragmentation sequence because it may
    /// have cached a link-layer destination selected under the previous mode.
    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    pub fn set_neighbor_discovery_enabled(&mut self, enabled: bool) {
        if self.inner.neighbor_discovery_enabled == enabled {
            return;
        }
        self.inner.neighbor_discovery_enabled = enabled;
        if !enabled {
            self.flush_neighbor_cache();
        } else {
            #[cfg(feature = "_proto-fragmentation")]
            self.fragmenter.reset();
        }
    }

    /// Get the IP addresses of the interface.
    pub fn ip_addrs(&self) -> &[IpCidr] {
        self.inner.ip_addrs.as_ref()
    }

    /// Get the first IPv4 address if present.
    #[cfg(feature = "proto-ipv4")]
    pub fn ipv4_addr(&self) -> Option<Ipv4Address> {
        self.inner.ipv4_addr()
    }

    /// Get the first IPv6 address if present.
    #[cfg(feature = "proto-ipv6")]
    pub fn ipv6_addr(&self) -> Option<Ipv6Address> {
        self.inner.ipv6_addr()
    }

    /// Get an address from the interface that could be used as source address. For IPv4, this is
    /// the first IPv4 address from the list of addresses. For IPv6, the address is based on the
    /// destination address and uses RFC6724 for selecting the source address.
    pub fn get_source_address(&self, dst_addr: &IpAddress) -> Option<IpAddress> {
        self.inner.get_source_address(dst_addr)
    }

    /// Get an address from the interface that could be used as source address. This is the first
    /// IPv4 address from the list of addresses in the interface.
    #[cfg(feature = "proto-ipv4")]
    pub fn get_source_address_ipv4(&self, dst_addr: &Ipv4Address) -> Option<Ipv4Address> {
        self.inner.get_source_address_ipv4(dst_addr)
    }

    /// Get an address from the interface that could be used as source address. The selection is
    /// based on RFC6724.
    #[cfg(feature = "proto-ipv6")]
    pub fn get_source_address_ipv6(&self, dst_addr: &Ipv6Address) -> Ipv6Address {
        self.inner.get_source_address_ipv6(dst_addr)
    }

    /// Update the IP addresses of the interface.
    ///
    /// # Panics
    /// This function panics if any of the addresses are not unicast.
    pub fn update_ip_addrs<F: FnOnce(&mut Vec<IpCidr, IFACE_MAX_ADDR_COUNT>)>(&mut self, f: F) {
        f(&mut self.inner.ip_addrs);
        InterfaceInner::flush_neighbor_cache(&mut self.inner);
        InterfaceInner::check_ip_addrs(&self.inner.ip_addrs);

        #[cfg(all(feature = "proto-ipv6", feature = "multicast"))]
        if self.inner.caps.medium == Medium::Ethernet {
            self.update_solicited_node_groups();
        }
    }

    /// Check whether the interface has the given IP address assigned.
    pub fn has_ip_addr<T: Into<IpAddress>>(&self, addr: T) -> bool {
        self.inner.has_ip_addr(addr)
    }

    pub fn routes(&self) -> &Routes {
        &self.inner.routes
    }

    pub fn routes_mut(&mut self) -> &mut Routes {
        &mut self.inner.routes
    }

    /// Select whether the route table includes connected prefixes.
    ///
    /// The default keeps smoltcp's traditional behavior where configured
    /// addresses implicitly make their subnet on-link. Set this to `true`
    /// when an external control plane installs connected routes explicitly;
    /// route removal will then make that subnet unreachable as expected.
    pub fn set_route_table_includes_connected_prefixes(&mut self, enabled: bool) {
        self.inner.route_table_includes_connected_prefixes = enabled;
    }

    /// Serialize an already-constructed IPv4 packet for an explicit next hop.
    ///
    /// This is intended for integrations whose authoritative FIB lives outside
    /// smoltcp. The supplied next hop is used directly for Ethernet neighbor
    /// resolution instead of consulting this interface's route table. When
    /// `destination_hardware_addr` is present, it represents an externally
    /// configured permanent neighbor and bypasses dynamic discovery.
    ///
    /// The transmit token may be consumed to emit either the IPv4 frame or an
    /// ARP request. On [`Ipv4PacketDispatchError::NeighborPending`], callers
    /// must retain the original IPv4 packet and retry it later.
    #[cfg(feature = "proto-ipv4")]
    pub fn dispatch_ipv4_packet<Tx: TxToken>(
        &mut self,
        timestamp: Instant,
        tx_token: Tx,
        next_hop: Ipv4Address,
        destination_hardware_addr: Option<HardwareAddress>,
        ip_packet: &[u8],
    ) -> Result<(), Ipv4PacketDispatchError> {
        self.dispatch_ip_packet(
            timestamp,
            tx_token,
            next_hop.into(),
            destination_hardware_addr,
            ip_packet,
        )
    }

    /// Send a complete IP packet using an explicit next hop, without a second
    /// route lookup. On NeighborPending the caller retains and retries it.
    pub fn dispatch_ip_packet<Tx: TxToken>(
        &mut self,
        timestamp: Instant,
        tx_token: Tx,
        next_hop: IpAddress,
        destination_hardware_addr: Option<HardwareAddress>,
        ip_packet: &[u8],
    ) -> Result<(), IpPacketDispatchError> {
        #[cfg(feature = "proto-ipv4")]
        let arp_source = Ipv4Packet::new_checked(ip_packet)
            .ok()
            .filter(|packet| packet.version() == 4)
            .map(|packet| IpAddress::Ipv4(packet.src_addr()))
            .filter(|source| self.inner.has_ip_addr(*source));
        #[cfg(not(feature = "proto-ipv4"))]
        let arp_source = None;
        self.dispatch_ip_packet_with_arp_source(
            timestamp,
            tx_token,
            next_hop,
            destination_hardware_addr,
            ip_packet,
            arp_source,
        )
    }

    /// Dispatch an already-routed packet with namespace-local ARP ownership.
    ///
    /// `arp_source` authorizes the packet's IPv4 source for ARP announcements
    /// even when that address belongs to another interface in the caller's
    /// network namespace. The caller must verify this ownership; remote
    /// forwarded sources must not be authorized. `None` explicitly requests
    /// interface-prefix fallback, including when the packet source is still
    /// configured but no longer classified as local by the caller's FIB.
    /// A hint different from the
    /// packet source is ignored. IPv6 neighbor discovery is unchanged.
    pub fn dispatch_ip_packet_with_arp_source<Tx: TxToken>(
        &mut self,
        timestamp: Instant,
        mut tx_token: Tx,
        next_hop: IpAddress,
        destination_hardware_addr: Option<HardwareAddress>,
        ip_packet: &[u8],
        arp_source: Option<IpAddress>,
    ) -> Result<(), IpPacketDispatchError> {
        let version =
            IpVersion::of_packet(ip_packet).map_err(|_| IpPacketDispatchError::Malformed)?;
        let (packet_len, packet_source) = match (version, next_hop) {
            #[cfg(feature = "proto-ipv4")]
            (IpVersion::Ipv4, IpAddress::Ipv4(_)) => {
                let packet = Ipv4Packet::new_checked(ip_packet)
                    .map_err(|_| IpPacketDispatchError::Malformed)?;
                (packet.total_len() as usize, Some(packet.src_addr().into()))
            }
            #[cfg(feature = "proto-ipv6")]
            (IpVersion::Ipv6, IpAddress::Ipv6(_)) => (
                IPV6_HEADER_LEN
                    + Ipv6Packet::new_checked(ip_packet)
                        .map_err(|_| IpPacketDispatchError::Malformed)?
                        .payload_len() as usize,
                None,
            ),
            #[allow(unreachable_patterns)]
            _ => return Err(IpPacketDispatchError::Malformed),
        };
        if packet_len != ip_packet.len() {
            return Err(Ipv4PacketDispatchError::Malformed);
        }
        let arp_source = packet_source.filter(|source| arp_source == Some(*source));

        self.inner.now = timestamp;
        match self.inner.caps.medium {
            #[cfg(feature = "medium-ip")]
            Medium::Ip => {
                tx_token
                    .prepare_buffer(packet_len)
                    .map_err(|_| IpPacketDispatchError::Exhausted)?;
                tx_token.consume(packet_len, |buffer| {
                    buffer.copy_from_slice(ip_packet);
                    Ok(())
                })
            }
            #[cfg(feature = "medium-ethernet")]
            Medium::Ethernet => {
                let (destination_hardware_addr, tx_token) =
                    if let Some(destination_hardware_addr) = destination_hardware_addr {
                        if destination_hardware_addr.medium() != Medium::Ethernet {
                            return Err(Ipv4PacketDispatchError::InvalidHardwareAddress);
                        }
                        (destination_hardware_addr.ethernet_or_panic(), tx_token)
                    } else {
                        let (hardware_addr, tx_token) =
                            match self.inner.lookup_hardware_addr_for_next_hop(
                                tx_token,
                                &next_hop,
                                arp_source,
                                &mut self.fragmenter,
                            ) {
                                Ok(result) => result,
                                Err(DispatchError::NeighborPending) => {
                                    return Err(Ipv4PacketDispatchError::NeighborPending {
                                        retry_at: self
                                            .inner
                                            .neighbor_cache
                                            .discovery_retry_at(&next_hop, timestamp),
                                    });
                                }
                                Err(DispatchError::NoRoute | DispatchError::MtuRetry(_)) => {
                                    return Err(Ipv4PacketDispatchError::NoRoute);
                                }
                                Err(DispatchError::PolicyDrop) => {
                                    return Err(Ipv4PacketDispatchError::NoRoute);
                                }
                                Err(DispatchError::Exhausted) => {
                                    return Err(Ipv4PacketDispatchError::Exhausted);
                                }
                            };
                        (hardware_addr.ethernet_or_panic(), tx_token)
                    };

                self.inner
                    .dispatch_ethernet(tx_token, packet_len, |mut frame| {
                        frame.set_dst_addr(destination_hardware_addr);
                        frame.set_ethertype(match version {
                            #[cfg(feature = "proto-ipv4")]
                            IpVersion::Ipv4 => EthernetProtocol::Ipv4,
                            #[cfg(feature = "proto-ipv6")]
                            IpVersion::Ipv6 => EthernetProtocol::Ipv6,
                        });
                        frame.payload_mut().copy_from_slice(ip_packet);
                    })
                    .map_err(|error| match error {
                        DispatchError::NoRoute | DispatchError::MtuRetry(_) => {
                            Ipv4PacketDispatchError::NoRoute
                        }
                        DispatchError::PolicyDrop => Ipv4PacketDispatchError::NoRoute,
                        DispatchError::Exhausted => Ipv4PacketDispatchError::Exhausted,
                        DispatchError::NeighborPending => {
                            Ipv4PacketDispatchError::NeighborPending {
                                retry_at: self
                                    .inner
                                    .neighbor_cache
                                    .discovery_retry_at(&next_hop, timestamp),
                            }
                        }
                    })
            }
            #[allow(unreachable_patterns)]
            _ => Err(Ipv4PacketDispatchError::NoRoute),
        }
    }

    /// Return whether dynamic neighbor discovery currently has a usable
    /// link-layer address for `protocol_addr`.
    ///
    /// External FIB integrations can use this to release packets queued after
    /// [`Ipv4PacketDispatchError::NeighborPending`] as soon as an ARP reply is
    /// processed, without waiting for the discovery retry deadline.
    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    pub fn is_neighbor_resolved(&mut self, timestamp: Instant, protocol_addr: IpAddress) -> bool {
        if !protocol_addr.is_unicast() {
            return false;
        }
        self.inner.now = timestamp;
        !self.inner.neighbor_discovery_enabled
            || self
                .inner
                .neighbor_cache
                .lookup(&protocol_addr, timestamp)
                .found()
    }

    /// Invalidate one dynamically learned neighbor and its discovery
    /// rate-limit state without disturbing unrelated cache entries.
    ///
    /// Control planes that temporarily override dynamic resolution can use
    /// this when removing the override, ensuring the next packet performs
    /// fresh neighbor discovery instead of reviving a stale mapping.
    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    pub fn invalidate_neighbor(&mut self, protocol_addr: IpAddress) {
        self.inner.neighbor_cache.remove(&protocol_addr);
    }

    /// Invalidate dynamic neighbor resolution state.
    ///
    /// This removes every dynamically learned mapping and discovery rate-limit
    /// entry. It also drops a pending fragmentation sequence because it may
    /// cache a link-layer destination selected before invalidation. Link-state
    /// integrations should call this when a link goes down so neither stale
    /// mappings nor pending output are reused after the link comes back up.
    /// Neighbor discovery policy is not changed. Calling this with no cached or
    /// pending state is harmless.
    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    pub fn flush_neighbor_cache(&mut self) {
        self.inner.neighbor_cache.flush();
        #[cfg(feature = "_proto-fragmentation")]
        self.fragmenter.reset();
    }

    /// Enable or disable the AnyIP capability.
    ///
    /// AnyIP allowins packets to be received
    /// locally on IP addresses other than the interface's configured [ip_addrs].
    /// When AnyIP is enabled and a route prefix in [`routes`](Self::routes) specifies one of
    /// the interface's [`ip_addrs`](Self::ip_addrs) as its gateway, the interface will accept
    /// packets addressed to that prefix.
    pub fn set_any_ip(&mut self, any_ip: bool) {
        self.inner.any_ip = any_ip;
    }

    /// Get whether AnyIP is enabled.
    ///
    /// See [`set_any_ip`](Self::set_any_ip) for details on AnyIP
    pub fn any_ip(&self) -> bool {
        self.inner.any_ip
    }

    /// Return whether IPv4 fragmentation still needs transmit tokens carrying
    /// an integration-provided egress override.
    pub fn has_pending_egress_override(&self) -> bool {
        #[cfg(feature = "proto-ipv4-fragmentation")]
        {
            !self.fragmenter.is_empty()
                && !self.fragmenter.finished()
                && self.fragmenter.ipv4.egress.is_some()
        }
        #[cfg(not(feature = "proto-ipv4-fragmentation"))]
        {
            false
        }
    }

    /// Get the packet reassembly timeout.
    #[cfg(feature = "_proto-fragmentation")]
    pub fn reassembly_timeout(&self) -> Duration {
        self.fragments.reassembly_timeout
    }

    /// Set the packet reassembly timeout.
    #[cfg(feature = "_proto-fragmentation")]
    pub fn set_reassembly_timeout(&mut self, timeout: Duration) {
        if timeout > Duration::from_secs(60) {
            net_debug!("RFC 4944 specifies that the reassembly timeout MUST be set to a maximum of 60 seconds");
        }
        self.fragments.reassembly_timeout = timeout;
    }

    /// Transmit packets queued in the sockets, and receive packets queued
    /// in the device.
    ///
    /// This function returns a value indicating whether the state of any socket
    /// might have changed.
    ///
    /// ## DoS warning
    ///
    /// This function processes all packets in the device's queue. This can
    /// be an unbounded amount of work if packets arrive faster than they're
    /// processed.
    ///
    /// If this is a concern for your application (i.e. your environment doesn't
    /// have preemptive scheduling, or `poll()` is called from a main loop where
    /// other important things are processed), you may use the lower-level methods
    /// [`poll_egress()`](Self::poll_egress) and [`poll_ingress_single()`](Self::poll_ingress_single).
    /// This allows you to insert yields or process other events between processing
    /// individual ingress packets.
    pub fn poll(
        &mut self,
        timestamp: Instant,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_, '_>,
    ) -> PollResult {
        self.poll_inner(
            timestamp,
            device,
            sockets,
            #[cfg(feature = "alloc")]
            None,
        )
    }

    /// Poll all ingress and egress with the same pre-routing decision as
    /// [`poll_ingress_single_filtered`](Self::poll_ingress_single_filtered).
    /// An integration that uses both polling styles must use their filtered
    /// variants for both, or some ingress packets will bypass its policy.
    #[cfg(feature = "alloc")]
    pub fn poll_filtered(
        &mut self,
        timestamp: Instant,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_, '_>,
        filter: &mut dyn IpIngressFilter,
    ) -> PollResult {
        self.poll_inner(timestamp, device, sockets, Some(filter))
    }

    fn poll_inner(
        &mut self,
        timestamp: Instant,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_, '_>,
        #[cfg(feature = "alloc")] mut filter: Option<&mut (dyn IpIngressFilter + '_)>,
    ) -> PollResult {
        self.inner.now = timestamp;

        let mut res = PollResult::None;

        #[cfg(feature = "_proto-fragmentation")]
        self.fragments.assembler.remove_expired(timestamp);

        // Process ingress while there's packets available.
        loop {
            #[cfg(feature = "alloc")]
            if filter
                .as_ref()
                .is_some_and(|filter| !filter.continue_ingress_poll())
            {
                return res;
            }
            match self.socket_ingress(
                device,
                sockets,
                #[cfg(feature = "alloc")]
                filter.as_deref_mut(),
            ) {
                PollIngressSingleResult::None => break,
                PollIngressSingleResult::PacketProcessed => {}
                PollIngressSingleResult::SocketStateChanged => res = PollResult::SocketStateChanged,
            }
        }

        // Process egress.
        #[cfg(feature = "alloc")]
        if filter
            .as_ref()
            .is_some_and(|filter| !filter.continue_ingress_poll())
        {
            return res;
        }
        match self.poll_egress(timestamp, device, sockets) {
            PollResult::None => {}
            PollResult::SocketStateChanged => res = PollResult::SocketStateChanged,
        }

        res
    }

    /// Transmit packets queued in the sockets.
    ///
    /// This function returns a value indicating whether the state of any socket
    /// might have changed.
    ///
    /// This is guaranteed to always perform a bounded amount of work.
    pub fn poll_egress(
        &mut self,
        timestamp: Instant,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_, '_>,
    ) -> PollResult {
        self.inner.now = timestamp;

        if !device.policy_current() {
            return PollResult::None;
        }

        match self.inner.caps.medium {
            #[cfg(feature = "medium-ieee802154")]
            Medium::Ieee802154 => {
                #[cfg(feature = "proto-sixlowpan-fragmentation")]
                self.sixlowpan_egress(device);
            }
            #[cfg(any(feature = "medium-ethernet", feature = "medium-ip"))]
            _ => {
                #[cfg(feature = "proto-ipv4-fragmentation")]
                self.ipv4_egress(device);
            }
        }

        #[cfg(feature = "multicast")]
        self.multicast_egress(device);

        self.socket_egress(device, sockets)
    }

    /// Process one incoming packet queued in the device.
    ///
    /// Returns a value indicating:
    /// - whether a packet was processed, in which case you have to call this method again in case there's more packets queued.
    /// - whether the state of any socket might have changed.
    ///
    /// Since it processes at most one packet, this is guaranteed to always perform a bounded amount of work.
    pub fn poll_ingress_single(
        &mut self,
        timestamp: Instant,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_, '_>,
    ) -> PollIngressSingleResult {
        self.inner.now = timestamp;
        #[cfg(all(feature = "alloc", feature = "socket-tcp"))]
        sockets.expire_tcp_time_wait(timestamp);

        #[cfg(feature = "_proto-fragmentation")]
        self.fragments.assembler.remove_expired(timestamp);

        self.socket_ingress(
            device,
            sockets,
            #[cfg(feature = "alloc")]
            None,
        )
    }

    /// Process one packet with an integration-owned pre-routing decision.
    #[cfg(feature = "alloc")]
    pub fn poll_ingress_single_filtered(
        &mut self,
        timestamp: Instant,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_, '_>,
        filter: &mut dyn IpIngressFilter,
    ) -> PollIngressSingleResult {
        self.inner.now = timestamp;
        #[cfg(feature = "socket-tcp")]
        sockets.expire_tcp_time_wait(timestamp);
        #[cfg(feature = "_proto-fragmentation")]
        self.fragments.assembler.remove_expired(timestamp);
        if !filter.continue_ingress_poll() {
            return PollIngressSingleResult::None;
        }
        self.socket_ingress(device, sockets, Some(filter))
    }

    /// Return a _soft deadline_ for calling [poll] the next time.
    /// The [Instant] returned is the time at which you should call [poll] next.
    /// It is harmless (but wastes energy) to call it before the [Instant], and
    /// potentially harmful (impacting quality of service) to call it after the
    /// [Instant]
    ///
    /// [poll]: #method.poll
    /// [Instant]: struct.Instant.html
    pub fn poll_at(&mut self, timestamp: Instant, sockets: &SocketSet<'_, '_>) -> Option<Instant> {
        self.inner.now = timestamp;

        #[cfg(feature = "_proto-fragmentation")]
        if !self.fragmenter.is_empty() {
            return Some(Instant::from_millis(0));
        }

        #[cfg(feature = "multicast")]
        let multicast_retry_at = self.inner.multicast.retry_at();
        #[cfg(not(feature = "multicast"))]
        let multicast_retry_at: Option<Instant> = None;

        let inner = &mut self.inner;

        let active = sockets
            .items()
            .filter_map(move |item| {
                let socket_poll_at = item.socket.poll_at(inner);
                match item
                    .meta
                    .poll_at(socket_poll_at, |ip_addr| inner.has_neighbor(&ip_addr))
                {
                    PollAt::Ingress => None,
                    PollAt::Time(instant) => Some(instant),
                    PollAt::Now => Some(Instant::from_millis(0)),
                }
            })
            .min();
        #[cfg(all(feature = "alloc", feature = "socket-tcp"))]
        {
            active
                .into_iter()
                .chain(multicast_retry_at)
                .chain(sockets.tcp_time_wait_poll_at())
                .min()
        }
        #[cfg(not(all(feature = "alloc", feature = "socket-tcp")))]
        {
            active.into_iter().chain(multicast_retry_at).min()
        }
    }

    /// Return an _advisory wait time_ for calling [poll] the next time.
    /// The [Duration] returned is the time left to wait before calling [poll] next.
    /// It is harmless (but wastes energy) to call it before the [Duration] has passed,
    /// and potentially harmful (impacting quality of service) to call it after the
    /// [Duration] has passed.
    ///
    /// [poll]: #method.poll
    /// [Duration]: struct.Duration.html
    pub fn poll_delay(
        &mut self,
        timestamp: Instant,
        sockets: &SocketSet<'_, '_>,
    ) -> Option<Duration> {
        match self.poll_at(timestamp, sockets) {
            Some(poll_at) if timestamp < poll_at => Some(poll_at - timestamp),
            Some(_) => Some(Duration::from_millis(0)),
            _ => None,
        }
    }

    /// Configure linear CPU packet storage before processing packets. This does
    /// not change device DMA storage. An active fragmented datagram is retained.
    #[cfg(feature = "alloc")]
    pub fn set_packet_buffer_allocator(
        &mut self,
        allocator: crate::storage::PacketBytesAllocator,
    ) -> Result<(), crate::storage::OwnedBufferResizeError> {
        #[cfg(feature = "_proto-fragmentation")]
        if !self.fragmenter.is_empty() {
            return Err(crate::storage::OwnedBufferResizeError);
        }
        #[cfg(feature = "_proto-fragmentation")]
        self.fragments.assembler.set_allocator(allocator)?;
        self.ingress_scratch = crate::storage::PacketBytes::with_allocator(allocator);
        #[cfg(feature = "_proto-fragmentation")]
        {
            self.fragmenter.buffer = crate::storage::PacketBytes::with_allocator(allocator);
        }
        Ok(())
    }

    fn socket_ingress(
        &mut self,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_, '_>,
        #[cfg(feature = "alloc")] mut filter: Option<&mut (dyn IpIngressFilter + '_)>,
    ) -> PollIngressSingleResult {
        let Some((rx_token, tx_token)) = device.receive(self.inner.now) else {
            return PollIngressSingleResult::None;
        };

        let rx_meta = rx_token.meta();
        let result = rx_token.consume(|frame| {
            #[cfg(feature = "alloc")]
            if let Some(filter) = filter.as_deref_mut() {
                filter.begin_packet(rx_meta);
            }
            if frame.is_empty() {
                return PollIngressSingleResult::PacketProcessed;
            }

            match self.inner.caps.medium {
                #[cfg(feature = "medium-ethernet")]
                Medium::Ethernet => {
                    #[cfg(feature = "alloc")]
                    let response = if let Some(filter) = filter {
                        self.inner.process_ethernet_filtered(
                            sockets,
                            rx_meta,
                            frame,
                            &mut self.fragments,
                            &mut self.ingress_scratch,
                            filter,
                        )
                    } else {
                        self.inner
                            .process_ethernet(sockets, rx_meta, frame, &mut self.fragments)
                    };
                    #[cfg(not(feature = "alloc"))]
                    let response =
                        self.inner
                            .process_ethernet(sockets, rx_meta, frame, &mut self.fragments);
                    if let Some(packet) = response {
                        if let Err(err) =
                            self.inner.dispatch(tx_token, packet, &mut self.fragmenter)
                        {
                            net_debug!("Failed to send response: {:?}", err);
                        }
                    }
                }
                #[cfg(feature = "medium-ip")]
                Medium::Ip => {
                    #[cfg(feature = "alloc")]
                    let response = if let Some(filter) = filter {
                        self.inner.process_ip_filtered(
                            sockets,
                            rx_meta,
                            frame,
                            &mut self.fragments,
                            &mut self.ingress_scratch,
                            filter,
                        )
                    } else {
                        self.inner
                            .process_ip(sockets, rx_meta, frame, &mut self.fragments)
                    };
                    #[cfg(not(feature = "alloc"))]
                    let response =
                        self.inner
                            .process_ip(sockets, rx_meta, frame, &mut self.fragments);
                    if let Some(packet) = response {
                        if let Err(err) = self.inner.dispatch_ip(
                            tx_token,
                            packet.tx_meta(),
                            packet,
                            &mut self.fragmenter,
                        ) {
                            net_debug!("Failed to send response: {:?}", err);
                        }
                    }
                }
                #[cfg(feature = "medium-ieee802154")]
                Medium::Ieee802154 => {
                    // 6LoWPAN needs its own post-decompression IP boundary.
                    // Until that exists, a filtered poll must fail closed
                    // rather than silently bypass the caller's policy.
                    #[cfg(feature = "alloc")]
                    if filter.is_some_and(|filter| filter.applies_to(IpVersion::Ipv6)) {
                        return PollIngressSingleResult::PacketProcessed;
                    }
                    if let Some(packet) =
                        self.inner
                            .process_ieee802154(sockets, rx_meta, frame, &mut self.fragments)
                    {
                        if let Err(err) = self.inner.dispatch_ip(
                            tx_token,
                            packet.tx_meta(),
                            packet,
                            &mut self.fragmenter,
                        ) {
                            net_debug!("Failed to send response: {:?}", err);
                        }
                    }
                }
            }

            // TODO: Propagate the PollIngressSingleResult from deeper.
            // There's many received packets that we process but can't cause sockets
            // to change state. For example IP fragments, multicast stuff, ICMP pings
            // if they dont't match any raw socket...
            // We should return `PacketProcessed` for these to save the user from
            // doing useless socket polls.
            PollIngressSingleResult::SocketStateChanged
        });
        #[cfg(feature = "alloc")]
        self.ingress_scratch.release_owned();
        result
    }

    fn socket_egress(
        &mut self,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_, '_>,
    ) -> PollResult {
        let _caps = device.capabilities();
        #[cfg(all(feature = "alloc", feature = "socket-tcp"))]
        sockets.expire_tcp_time_wait(self.inner.now);

        enum EgressError {
            MtuRetry(usize),
            Exhausted,
            Dispatch,
        }

        let mut result = PollResult::None;
        for item in sockets.items_mut() {
            // A changed policy may require a different route, interface name,
            // or NAT view. Leave this socket and every later one untouched;
            // the integration will restart the poll with a fresh snapshot.
            if !device.policy_current() {
                break;
            }
            if !item
                .meta
                .egress_permitted(self.inner.now, |ip_addr| self.inner.has_neighbor(&ip_addr))
            {
                continue;
            }

            let mut neighbor_addr = None;
            let policy_dropped = core::cell::Cell::new(false);
            #[cfg(feature = "socket-tcp")]
            #[allow(unreachable_patterns)]
            let tcp_mtu =
                match &item.socket {
                    Socket::Tcp(socket) => socket
                        .local_endpoint()
                        .zip(socket.remote_endpoint())
                        .map(|(local, remote)| {
                            device.outbound_tcp_mtu(local, remote, socket.egress_meta())
                        }),
                    _ => None,
                };
            #[cfg(feature = "socket-tcp")]
            let tcp_mtu_generation = device.outbound_tcp_mtu_generation();
            let mut respond = |inner: &mut InterfaceInner, meta: PacketMeta, response: Packet| {
                neighbor_addr = Some(response.ip_repr().dst_addr());
                let t = device.transmit(inner.now).ok_or_else(|| {
                    net_debug!("failed to transmit IP: device exhausted");
                    EgressError::Exhausted
                })?;

                match inner.dispatch_ip(t, meta, response, &mut self.fragmenter) {
                    Ok(()) => {}
                    Err(DispatchError::PolicyDrop) => policy_dropped.set(true),
                    Err(DispatchError::Exhausted) => return Err(EgressError::Exhausted),
                    Err(DispatchError::MtuRetry(mtu)) => return Err(EgressError::MtuRetry(mtu)),
                    Err(DispatchError::NoRoute | DispatchError::NeighborPending) => {
                        return Err(EgressError::Dispatch);
                    }
                }

                result = PollResult::SocketStateChanged;

                Ok(())
            };

            let result = match &mut item.socket {
                #[cfg(feature = "socket-raw")]
                Socket::Raw(socket) => socket.dispatch(&mut self.inner, |inner, (ip, raw)| {
                    respond(
                        inner,
                        PacketMeta::default(),
                        Packet::new(ip, IpPayload::Raw(raw)),
                    )
                }),
                #[cfg(feature = "socket-icmp")]
                Socket::Icmp(socket) => {
                    socket.dispatch(&mut self.inner, |inner, response| match response {
                        #[cfg(feature = "proto-ipv4")]
                        (IpRepr::Ipv4(ipv4_repr), IcmpRepr::Ipv4(icmpv4_repr)) => respond(
                            inner,
                            PacketMeta::default(),
                            Packet::new_ipv4(ipv4_repr, IpPayload::Icmpv4(icmpv4_repr)),
                        ),
                        #[cfg(feature = "proto-ipv6")]
                        (IpRepr::Ipv6(ipv6_repr), IcmpRepr::Ipv6(icmpv6_repr)) => respond(
                            inner,
                            PacketMeta::default(),
                            Packet::new_ipv6(ipv6_repr, IpPayload::Icmpv6(icmpv6_repr)),
                        ),
                        #[allow(unreachable_patterns)]
                        _ => unreachable!(),
                    })
                }
                #[cfg(feature = "socket-udp")]
                Socket::Udp(socket) => {
                    socket.dispatch(&mut self.inner, |inner, meta, (ip, udp, payload)| {
                        respond(inner, meta, Packet::new(ip, IpPayload::Udp(udp, payload)))
                    })
                }
                #[cfg(feature = "socket-tcp")]
                Socket::Tcp(socket) => {
                    let meta = socket.egress_meta();
                    let mtu = tcp_mtu.unwrap_or_else(|| self.inner.ip_mtu());
                    socket
                        .dispatch_with_mtu_policy_generation(
                            &mut self.inner,
                            mtu,
                            tcp_mtu_generation,
                            |inner, (ip, tcp)| {
                                match respond(inner, meta, Packet::new(ip, IpPayload::Tcp(tcp))) {
                                    Err(EgressError::MtuRetry(mtu)) => {
                                        return Ok(
                                            crate::socket::tcp::TcpPacketAdmission::MtuRetry(mtu),
                                        )
                                    }
                                    result => result?,
                                }
                                Ok(if policy_dropped.replace(false) {
                                    crate::socket::tcp::TcpPacketAdmission::PolicyDropped
                                } else {
                                    crate::socket::tcp::TcpPacketAdmission::Admitted
                                })
                            },
                        )
                        .map(|_| ())
                }
                #[cfg(feature = "socket-dhcpv4")]
                Socket::Dhcpv4(socket) => {
                    socket.dispatch(&mut self.inner, |inner, (ip, udp, dhcp)| {
                        respond(
                            inner,
                            PacketMeta::default(),
                            Packet::new_ipv4(ip, IpPayload::Dhcpv4(udp, dhcp)),
                        )
                    })
                }
                #[cfg(feature = "socket-dns")]
                Socket::Dns(socket) => socket.dispatch(&mut self.inner, |inner, (ip, udp, dns)| {
                    respond(
                        inner,
                        PacketMeta::default(),
                        Packet::new(ip, IpPayload::Udp(udp, dns)),
                    )
                }),
            };

            match result {
                Err(EgressError::Exhausted) => break, // Device buffer full.
                Err(EgressError::Dispatch) => {
                    // `NeighborCache` already takes care of rate limiting the neighbor discovery
                    // requests from the socket. However, without an additional rate limiting
                    // mechanism, we would spin on every socket that has yet to discover its
                    // neighbor.
                    item.meta.neighbor_missing(
                        self.inner.now,
                        neighbor_addr.expect("non-IP response packet"),
                    );
                }
                Err(EgressError::MtuRetry(_)) => {}
                Ok(()) => {}
            }
        }
        result
    }
}

impl InterfaceInner {
    #[allow(unused)] // unused depending on which sockets are enabled
    pub fn now(&self) -> Instant {
        self.now
    }

    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    #[allow(unused)] // unused depending on which sockets are enabled
    pub fn hardware_addr(&self) -> HardwareAddress {
        self.hardware_addr
    }

    #[allow(unused)] // unused depending on which sockets are enabled
    pub fn checksum_caps(&self) -> ChecksumCapabilities {
        self.caps.checksum.clone()
    }

    #[allow(unused)] // unused depending on which sockets are enabled
    pub fn ip_mtu(&self) -> usize {
        self.caps.ip_mtu()
    }

    #[allow(unused)] // unused depending on which sockets are enabled, and in tests
    pub fn rand(&mut self) -> &mut Rand {
        &mut self.rand
    }

    #[allow(unused)] // unused depending on which sockets are enabled
    pub fn get_source_address(&self, dst_addr: &IpAddress) -> Option<IpAddress> {
        match dst_addr {
            #[cfg(feature = "proto-ipv4")]
            IpAddress::Ipv4(addr) => self.get_source_address_ipv4(addr).map(|a| a.into()),
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(addr) => Some(self.get_source_address_ipv6(addr).into()),
        }
    }

    #[cfg(test)]
    #[allow(unused)] // unused depending on which sockets are enabled
    pub fn set_now(&mut self, now: Instant) {
        self.now = now
    }

    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    fn check_hardware_addr(addr: &HardwareAddress) {
        if !addr.is_unicast() {
            panic!("Hardware address {addr} is not unicast")
        }
    }

    fn check_ip_addrs(addrs: &[IpCidr]) {
        for cidr in addrs {
            if !cidr.address().is_unicast() && !cidr.address().is_unspecified() {
                panic!("IP address {} is not unicast", cidr.address())
            }
        }
    }

    /// Check whether the interface has the given IP address assigned.
    fn has_ip_addr<T: Into<IpAddress>>(&self, addr: T) -> bool {
        let addr = addr.into();
        self.ip_addrs.iter().any(|probe| probe.address() == addr)
    }

    /// Check whether the interface listens to given destination multicast IP address.
    fn has_multicast_group<T: Into<IpAddress>>(&self, addr: T) -> bool {
        let addr = addr.into();

        #[cfg(feature = "multicast")]
        if self.multicast.has_multicast_group(addr) {
            return true;
        }

        match addr {
            #[cfg(feature = "proto-ipv4")]
            IpAddress::Ipv4(key) => key == IPV4_MULTICAST_ALL_SYSTEMS,
            #[cfg(feature = "proto-rpl")]
            IpAddress::Ipv6(IPV6_LINK_LOCAL_ALL_RPL_NODES) => true,
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(key) => {
                key == IPV6_LINK_LOCAL_ALL_NODES || self.has_solicited_node(key)
            }
            #[allow(unreachable_patterns)]
            _ => false,
        }
    }

    #[cfg(feature = "medium-ip")]
    fn process_ip<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        meta: PacketMeta,
        ip_payload: &'frame [u8],
        frag: &'frame mut FragmentsBuffer,
    ) -> Option<Packet<'frame>> {
        self.process_ip_inner(
            sockets,
            meta,
            ip_payload,
            frag,
            #[cfg(feature = "alloc")]
            None,
        )
    }

    #[cfg(all(feature = "alloc", feature = "medium-ip"))]
    fn process_ip_filtered<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        meta: PacketMeta,
        ip_payload: &'frame [u8],
        frag: &'frame mut FragmentsBuffer,
        scratch: &'frame mut crate::storage::PacketBytes,
        filter: &mut dyn IpIngressFilter,
    ) -> Option<Packet<'frame>> {
        self.process_ip_inner(sockets, meta, ip_payload, frag, Some((scratch, filter)))
    }

    #[cfg(feature = "medium-ip")]
    fn process_ip_inner<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        meta: PacketMeta,
        ip_payload: &'frame [u8],
        frag: &'frame mut FragmentsBuffer,
        #[cfg(feature = "alloc")] filter: Option<(
            &'frame mut crate::storage::PacketBytes,
            &mut dyn IpIngressFilter,
        )>,
    ) -> Option<Packet<'frame>> {
        match IpVersion::of_packet(ip_payload) {
            #[cfg(feature = "proto-ipv4")]
            Ok(IpVersion::Ipv4) => {
                let ipv4_packet = check!(Ipv4Packet::new_checked(ip_payload));
                #[cfg(feature = "alloc")]
                if let Some((scratch, filter)) = filter.filter(|(_, filter)| {
                    filter.applies_to_packet(IpVersion::Ipv4, ipv4_packet.as_ref())
                }) {
                    return self.process_ipv4_filtered(
                        sockets,
                        meta,
                        HardwareAddress::Ip,
                        &ipv4_packet,
                        frag,
                        scratch,
                        filter,
                    );
                }
                self.process_ipv4(sockets, meta, HardwareAddress::Ip, &ipv4_packet, frag)
            }
            #[cfg(feature = "proto-ipv6")]
            Ok(IpVersion::Ipv6) => {
                let ipv6_packet = check!(Ipv6Packet::new_checked(ip_payload));
                #[cfg(feature = "alloc")]
                if let Some((scratch, filter)) = filter.filter(|(_, filter)| {
                    filter.applies_to_packet(IpVersion::Ipv6, ipv6_packet.as_ref())
                }) {
                    return self.process_ipv6_filtered(
                        sockets,
                        meta,
                        HardwareAddress::Ip,
                        &ipv6_packet,
                        scratch,
                        filter,
                    );
                }
                self.process_ipv6(sockets, meta, HardwareAddress::Ip, &ipv6_packet)
            }
            // Drop all other traffic.
            _ => None,
        }
    }

    #[cfg(feature = "socket-raw")]
    fn raw_socket_filter(
        &mut self,
        sockets: &mut SocketSet,
        ip_repr: &IpRepr,
        ip_payload: &[u8],
    ) -> bool {
        let mut handled_by_raw_socket = false;

        // Pass every IP packet to all raw sockets we have registered.
        for raw_socket in sockets
            .items_mut()
            .filter_map(|i| raw::Socket::downcast_mut(&mut i.socket))
        {
            if raw_socket.accepts(ip_repr) {
                raw_socket.process(self, ip_repr, ip_payload);
                handled_by_raw_socket = true;
            }
        }
        handled_by_raw_socket
    }

    /// Checks if an address is broadcast, taking into account ipv4 subnet-local
    /// broadcast addresses.
    pub fn is_broadcast(&self, address: &IpAddress) -> bool {
        match address {
            #[cfg(feature = "proto-ipv4")]
            IpAddress::Ipv4(address) => self.is_broadcast_v4(*address),
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(_) => false,
        }
    }

    #[cfg(feature = "medium-ethernet")]
    fn dispatch<Tx>(
        &mut self,
        tx_token: Tx,
        packet: EthernetPacket,
        frag: &mut Fragmenter,
    ) -> Result<(), DispatchError>
    where
        Tx: TxToken,
    {
        match packet {
            #[cfg(feature = "proto-ipv4")]
            EthernetPacket::Arp(arp_repr) => {
                let dst_hardware_addr = match arp_repr {
                    ArpRepr::EthernetIpv4 {
                        target_hardware_addr,
                        ..
                    } => target_hardware_addr,
                };

                self.dispatch_ethernet(tx_token, arp_repr.buffer_len(), |mut frame| {
                    frame.set_dst_addr(dst_hardware_addr);
                    frame.set_ethertype(EthernetProtocol::Arp);

                    let mut packet = ArpPacket::new_unchecked(frame.payload_mut());
                    arp_repr.emit(&mut packet);
                })
            }
            EthernetPacket::Ip(packet) => {
                self.dispatch_ip(tx_token, packet.tx_meta(), packet, frag)
            }
        }
    }

    pub fn in_same_network(&self, addr: &IpAddress) -> bool {
        self.ip_addrs.iter().any(|cidr| cidr.contains_addr(addr))
    }

    pub fn route(&self, addr: &IpAddress, timestamp: Instant) -> Option<IpAddress> {
        // Broadcast traffic is always sent directly. Unicast traffic, including
        // destinations covered by an interface address, follows the explicit
        // route table so that a more-specific gateway route can win.
        if self.is_broadcast(addr) {
            return Some(*addr);
        }

        self.routes.lookup(addr, timestamp).or_else(|| {
            (!self.route_table_includes_connected_prefixes && self.in_same_network(addr))
                .then_some(*addr)
        })
    }

    fn has_neighbor(&self, addr: &IpAddress) -> bool {
        match self.route(addr, self.now) {
            Some(_routed_addr) => match self.caps.medium {
                #[cfg(feature = "medium-ethernet")]
                Medium::Ethernet => {
                    !self.neighbor_discovery_enabled
                        || self.neighbor_cache.lookup(&_routed_addr, self.now).found()
                }
                #[cfg(feature = "medium-ieee802154")]
                Medium::Ieee802154 => {
                    !self.neighbor_discovery_enabled
                        || self.neighbor_cache.lookup(&_routed_addr, self.now).found()
                }
                #[cfg(feature = "medium-ip")]
                Medium::Ip => true,
            },
            None => false,
        }
    }

    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    fn lookup_hardware_addr<Tx>(
        &mut self,
        tx_token: Tx,
        dst_addr: &IpAddress,
        arp_source: Option<IpAddress>,
        fragmenter: &mut Fragmenter,
    ) -> Result<(HardwareAddress, Tx), DispatchError>
    where
        Tx: TxToken,
    {
        let next_hop = if self.is_broadcast(dst_addr) || dst_addr.is_multicast() {
            *dst_addr
        } else {
            self.route(dst_addr, self.now)
                .ok_or(DispatchError::NoRoute)?
        };
        self.lookup_hardware_addr_for_next_hop(tx_token, &next_hop, arp_source, fragmenter)
    }

    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    fn lookup_hardware_addr_for_next_hop<Tx>(
        &mut self,
        tx_token: Tx,
        next_hop: &IpAddress,
        _arp_source: Option<IpAddress>,
        fragmenter: &mut Fragmenter,
    ) -> Result<(HardwareAddress, Tx), DispatchError>
    where
        Tx: TxToken,
    {
        if self.is_broadcast(next_hop) {
            let hardware_addr = match self.caps.medium {
                #[cfg(feature = "medium-ethernet")]
                Medium::Ethernet => HardwareAddress::Ethernet(EthernetAddress::BROADCAST),
                #[cfg(feature = "medium-ieee802154")]
                Medium::Ieee802154 => HardwareAddress::Ieee802154(Ieee802154Address::BROADCAST),
                #[cfg(feature = "medium-ip")]
                Medium::Ip => unreachable!(),
            };

            return Ok((hardware_addr, tx_token));
        }

        if next_hop.is_multicast() {
            let hardware_addr = match *next_hop {
                #[cfg(feature = "proto-ipv4")]
                IpAddress::Ipv4(addr) => match self.caps.medium {
                    #[cfg(feature = "medium-ethernet")]
                    Medium::Ethernet => {
                        let b = addr.octets();
                        HardwareAddress::Ethernet(EthernetAddress::from_bytes(&[
                            0x01,
                            0x00,
                            0x5e,
                            b[1] & 0x7F,
                            b[2],
                            b[3],
                        ]))
                    }
                    #[cfg(feature = "medium-ieee802154")]
                    Medium::Ieee802154 => unreachable!(),
                    #[cfg(feature = "medium-ip")]
                    Medium::Ip => unreachable!(),
                },
                #[cfg(feature = "proto-ipv6")]
                IpAddress::Ipv6(addr) => match self.caps.medium {
                    #[cfg(feature = "medium-ethernet")]
                    Medium::Ethernet => {
                        let b = addr.octets();
                        HardwareAddress::Ethernet(EthernetAddress::from_bytes(&[
                            0x33, 0x33, b[12], b[13], b[14], b[15],
                        ]))
                    }
                    #[cfg(feature = "medium-ieee802154")]
                    Medium::Ieee802154 => {
                        // Not sure if this is correct
                        HardwareAddress::Ieee802154(Ieee802154Address::BROADCAST)
                    }
                    #[cfg(feature = "medium-ip")]
                    Medium::Ip => unreachable!(),
                },
            };

            return Ok((hardware_addr, tx_token));
        }

        if !self.neighbor_discovery_enabled {
            return Ok((self.hardware_addr, tx_token));
        }

        match self.neighbor_cache.lookup(next_hop, self.now) {
            NeighborAnswer::Found(hardware_addr) => return Ok((hardware_addr, tx_token)),
            NeighborAnswer::RateLimited => return Err(DispatchError::NeighborPending),
            _ => (), // XXX
        }

        match *next_hop {
            #[cfg(all(feature = "medium-ethernet", feature = "proto-ipv4"))]
            IpAddress::Ipv4(dst_addr) if matches!(self.caps.medium, Medium::Ethernet) => {
                net_debug!(
                    "address {} not in neighbor cache, sending ARP request",
                    dst_addr
                );
                let src_hardware_addr = self.hardware_addr.ethernet_or_panic();

                let arp_repr = ArpRepr::EthernetIpv4 {
                    operation: ArpOperation::Request,
                    source_hardware_addr: src_hardware_addr,
                    source_protocol_addr: self
                        .get_arp_source_address(dst_addr, _arp_source)
                        .ok_or(DispatchError::NoRoute)?,
                    target_hardware_addr: EthernetAddress::BROADCAST,
                    target_protocol_addr: dst_addr,
                };

                if let Err(e) =
                    self.dispatch_ethernet(tx_token, arp_repr.buffer_len(), |mut frame| {
                        frame.set_dst_addr(EthernetAddress::BROADCAST);
                        frame.set_ethertype(EthernetProtocol::Arp);

                        arp_repr.emit(&mut ArpPacket::new_unchecked(frame.payload_mut()))
                    })
                {
                    net_debug!("Failed to dispatch ARP request: {:?}", e);
                    return Err(DispatchError::NeighborPending);
                }
            }

            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(dst_addr) => {
                net_debug!(
                    "address {} not in neighbor cache, sending Neighbor Solicitation",
                    dst_addr
                );

                let solicit = Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit {
                    target_addr: dst_addr,
                    lladdr: Some(self.hardware_addr.into()),
                });

                let packet = Packet::new_ipv6(
                    Ipv6Repr {
                        src_addr: self.get_source_address_ipv6(&dst_addr),
                        dst_addr: dst_addr.solicited_node(),
                        next_header: IpProtocol::Icmpv6,
                        payload_len: solicit.buffer_len(),
                        hop_limit: 0xff,
                    },
                    IpPayload::Icmpv6(solicit),
                );

                if let Err(e) =
                    self.dispatch_ip(tx_token, PacketMeta::default(), packet, fragmenter)
                {
                    net_debug!("Failed to dispatch NDISC solicit: {:?}", e);
                    return Err(DispatchError::NeighborPending);
                }
            }

            #[allow(unreachable_patterns)]
            _ => (),
        }

        // The request got dispatched, limit the rate on the cache.
        self.neighbor_cache.limit_rate(*next_hop, self.now);
        Err(DispatchError::NeighborPending)
    }

    fn flush_neighbor_cache(&mut self) {
        #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
        self.neighbor_cache.flush()
    }

    /// Get the neighbor cache.
    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    pub fn neighbor_cache(&self) -> &NeighborCache {
        &self.neighbor_cache
    }

    fn dispatch_ip<Tx: TxToken>(
        &mut self,
        // NOTE(unused_mut): tx_token isn't always mutated, depending on
        // the feature set that is used.
        #[allow(unused_mut)] mut tx_token: Tx,
        meta: PacketMeta,
        packet: Packet,
        frag: &mut Fragmenter,
    ) -> Result<(), DispatchError> {
        let mut ip_repr = packet.ip_repr();
        assert!(!ip_repr.dst_addr().is_unspecified());
        // Neighbor discovery belongs to the current link, including unicast
        // advertisements. It must not follow an external namespace route.
        let link_local_control = match &packet {
            #[cfg(all(
                feature = "proto-ipv6",
                any(feature = "medium-ethernet", feature = "medium-ieee802154")
            ))]
            Packet::Ipv6(packet) => matches!(
                packet.payload,
                IpPayload::Icmpv6(Icmpv6Repr::Ndisc(_))
                    | IpPayload::HopByHopIcmpv6(_, Icmpv6Repr::Ndisc(_))
            ),
            #[allow(unreachable_patterns)]
            _ => false,
        };
        if tx_token.deferred_ip_output(ip_repr.version()) {
            let len = ip_repr.buffer_len();
            match ip_repr {
                #[cfg(feature = "proto-ipv4")]
                IpRepr::Ipv4(_) if len > u16::MAX as usize => {
                    return Err(DispatchError::NoRoute);
                }
                #[cfg(feature = "proto-ipv6")]
                IpRepr::Ipv6(_) if len - ip_repr.header_len() > u16::MAX as usize => {
                    return Err(DispatchError::NoRoute);
                }
                _ => {}
            }
            let class = if link_local_control {
                IpOutputClass::LinkLocalControl
            } else {
                IpOutputClass::Ordinary
            };
            #[cfg(feature = "proto-ipv4-fragmentation")]
            let ipv4_fragment_ident =
                matches!(&ip_repr, IpRepr::Ipv4(_)).then(|| self.next_ipv4_frag_ident());
            #[cfg(not(feature = "proto-ipv4-fragmentation"))]
            let ipv4_fragment_ident = None;
            return tx_token
                .consume_full_ip(len, meta, class, ipv4_fragment_ident, |buffer| {
                    ip_repr.emit(&mut *buffer, &self.caps.checksum);
                    packet.emit_payload(&ip_repr, &mut buffer[ip_repr.header_len()..], &self.caps);
                })
                .map_err(|error| match error {
                    IpOutputError::Exhausted => DispatchError::Exhausted,
                    IpOutputError::PolicyDrop => DispatchError::PolicyDrop,
                    IpOutputError::MtuRetry(mtu) => DispatchError::MtuRetry(mtu),
                    IpOutputError::Unsupported
                    | IpOutputError::NoRoute
                    | IpOutputError::MtuExceeded => DispatchError::NoRoute,
                });
        }
        let egress = if link_local_control {
            None
        } else {
            tx_token
                .egress_override(ip_repr.version(), ip_repr.dst_addr(), meta)
                .map_err(|_| DispatchError::Exhausted)?
        };
        let tx_medium = egress.map_or(self.caps.medium, |egress| egress.medium);
        let ip_mtu = egress.map_or(self.caps.ip_mtu(), |egress| egress.ip_mtu);
        let arp_source = self
            .has_ip_addr(ip_repr.src_addr())
            .then_some(ip_repr.src_addr());

        // Dispatch IEEE802.15.4:

        #[cfg(feature = "medium-ieee802154")]
        if matches!(tx_medium, Medium::Ieee802154) {
            let (addr, tx_token) = if link_local_control {
                self.lookup_hardware_addr_for_next_hop(
                    tx_token,
                    &ip_repr.dst_addr(),
                    arp_source,
                    frag,
                )?
            } else {
                self.lookup_hardware_addr(tx_token, &ip_repr.dst_addr(), arp_source, frag)?
            };
            let addr = addr.ieee802154_or_panic();

            return self.dispatch_ieee802154(addr, tx_token, meta, packet, frag);
        }

        // Dispatch IP/Ethernet:

        let caps = self.caps.clone();

        #[cfg(feature = "proto-ipv4-fragmentation")]
        let ipv4_id = self.next_ipv4_frag_ident();

        // First we calculate the total length that we will have to emit.
        let mut total_len = ip_repr.buffer_len();

        // Add the size of the Ethernet header if the medium is Ethernet.
        #[cfg(feature = "medium-ethernet")]
        if matches!(tx_medium, Medium::Ethernet) {
            total_len = EthernetFrame::<&[u8]>::buffer_len(total_len);
        }

        // If the medium is Ethernet, then we need to retrieve the destination hardware address.
        #[cfg(feature = "medium-ethernet")]
        let (dst_hardware_addr, mut tx_token) = match tx_medium {
            Medium::Ethernet => {
                let neighbor = if link_local_control {
                    self.lookup_hardware_addr_for_next_hop(
                        tx_token,
                        &ip_repr.dst_addr(),
                        arp_source,
                        frag,
                    )?
                } else {
                    self.lookup_hardware_addr(tx_token, &ip_repr.dst_addr(), arp_source, frag)?
                };
                match neighbor {
                    (HardwareAddress::Ethernet(addr), tx_token) => (addr, tx_token),
                    (_, _) => unreachable!(),
                }
            }
            _ => (EthernetAddress([0; 6]), tx_token),
        };

        // Emit function for the Ethernet header.
        #[cfg(feature = "medium-ethernet")]
        let emit_ethernet = |repr: &IpRepr, tx_buffer: &mut [u8]| {
            let mut frame = EthernetFrame::new_unchecked(tx_buffer);

            let src_addr = self.hardware_addr.ethernet_or_panic();
            frame.set_src_addr(src_addr);
            frame.set_dst_addr(dst_hardware_addr);

            match repr.version() {
                #[cfg(feature = "proto-ipv4")]
                IpVersion::Ipv4 => frame.set_ethertype(EthernetProtocol::Ipv4),
                #[cfg(feature = "proto-ipv6")]
                IpVersion::Ipv6 => frame.set_ethertype(EthernetProtocol::Ipv6),
            }

            Ok(())
        };

        // Emit function for the IP header and payload.
        let emit_ip = |repr: &IpRepr, tx_buffer: &mut [u8]| {
            repr.emit(&mut *tx_buffer, &self.caps.checksum);

            let payload = &mut tx_buffer[repr.header_len()..];
            packet.emit_payload(repr, payload, &caps)
        };

        let total_ip_len = ip_repr.buffer_len();

        match &mut ip_repr {
            #[cfg(feature = "proto-ipv4")]
            IpRepr::Ipv4(repr) => {
                // If we have an IPv4 packet, then we need to check if we need to fragment it.
                if total_ip_len > ip_mtu {
                    #[cfg(feature = "proto-ipv4-fragmentation")]
                    {
                        net_debug!("start fragmentation");

                        let ip_header_len = repr.buffer_len();
                        if ip_mtu < ip_header_len {
                            net_debug!("egress IP MTU is smaller than the IPv4 header; dropping");
                            return Ok(());
                        }
                        let max_payload_len = ip_mtu - ip_header_len;
                        let first_payload_len = max_payload_len & !7;
                        if first_payload_len == 0 {
                            net_debug!("egress IP MTU cannot hold an aligned IPv4 fragment");
                            return Ok(());
                        }
                        let first_frag_ip_len = ip_header_len + first_payload_len;
                        // Calculate how much we will send now (including the Ethernet header).
                        let tx_len = first_frag_ip_len;
                        #[cfg(feature = "medium-ethernet")]
                        let tx_len = if matches!(tx_medium, Medium::Ethernet) {
                            tx_len + EthernetFrame::<&[u8]>::header_len()
                        } else {
                            tx_len
                        };

                        tx_token
                            .prepare_buffer(tx_len)
                            .map_err(|_| DispatchError::Exhausted)?;
                        if !frag.prepare_buffer(total_ip_len) {
                            net_debug!(
                                "Fragmentation buffer is too small, at least {} needed. Dropping",
                                total_ip_len
                            );
                            #[cfg(feature = "alloc")]
                            return Err(DispatchError::Exhausted);
                            #[cfg(not(feature = "alloc"))]
                            return Ok(());
                        }

                        #[cfg(feature = "medium-ethernet")]
                        {
                            frag.ipv4.dst_hardware_addr = dst_hardware_addr;
                        }

                        // Save the total packet len (without the Ethernet header, but with the first
                        // IP header).
                        frag.packet_len = total_ip_len;

                        // Save the IP header for other fragments.
                        frag.ipv4.repr = *repr;
                        frag.ipv4.meta = meta;
                        frag.ipv4.egress = egress;

                        // Save how much bytes we will send now.
                        frag.sent_bytes = first_frag_ip_len;

                        // Modify the IP header
                        repr.payload_len = first_frag_ip_len - repr.buffer_len();

                        // Emit the IP header to the buffer.
                        emit_ip(&ip_repr, &mut frag.buffer);

                        let mut ipv4_packet = Ipv4Packet::new_unchecked(&mut frag.buffer[..]);
                        frag.ipv4.ident = ipv4_id;
                        ipv4_packet.set_ident(ipv4_id);
                        ipv4_packet.set_more_frags(true);
                        ipv4_packet.set_dont_frag(false);
                        ipv4_packet.set_frag_offset(0);

                        if caps.checksum.ipv4.tx() {
                            ipv4_packet.fill_checksum();
                        }

                        // Transmit the first packet.
                        tx_token.set_meta(meta);
                        tx_token.consume(tx_len, |mut tx_buffer| {
                            #[cfg(feature = "medium-ethernet")]
                            if matches!(tx_medium, Medium::Ethernet) {
                                emit_ethernet(&ip_repr, tx_buffer)?;
                                tx_buffer = &mut tx_buffer[EthernetFrame::<&[u8]>::header_len()..];
                            }

                            // Change the offset for the next packet.
                            frag.ipv4.frag_offset = (first_frag_ip_len - ip_header_len) as u16;

                            // Copy the IP header and the payload.
                            tx_buffer[..first_frag_ip_len]
                                .copy_from_slice(&frag.buffer[..first_frag_ip_len]);

                            Ok(())
                        })
                    }

                    #[cfg(not(feature = "proto-ipv4-fragmentation"))]
                    {
                        net_debug!("Enable the `proto-ipv4-fragmentation` feature for fragmentation support.");
                        Ok(())
                    }
                } else {
                    tx_token.set_meta(meta);

                    // No fragmentation is required.
                    tx_token
                        .prepare_buffer(total_len)
                        .map_err(|_| DispatchError::Exhausted)?;
                    tx_token.consume(total_len, |mut tx_buffer| {
                        #[cfg(feature = "medium-ethernet")]
                        if matches!(tx_medium, Medium::Ethernet) {
                            emit_ethernet(&ip_repr, tx_buffer)?;
                            tx_buffer = &mut tx_buffer[EthernetFrame::<&[u8]>::header_len()..];
                        }

                        emit_ip(&ip_repr, tx_buffer);
                        Ok(())
                    })
                }
            }
            // We don't support IPv6 fragmentation yet.
            #[cfg(feature = "proto-ipv6")]
            IpRepr::Ipv6(_) => {
                if total_ip_len > ip_mtu {
                    // Source fragmentation is not implemented. Never hand an
                    // oversized packet to a bounded device buffer.
                    return Err(DispatchError::NoRoute);
                }
                tx_token.set_meta(meta);
                tx_token
                    .prepare_buffer(total_len)
                    .map_err(|_| DispatchError::Exhausted)?;
                tx_token.consume(total_len, |mut tx_buffer| {
                    #[cfg(feature = "medium-ethernet")]
                    if matches!(tx_medium, Medium::Ethernet) {
                        emit_ethernet(&ip_repr, tx_buffer)?;
                        tx_buffer = &mut tx_buffer[EthernetFrame::<&[u8]>::header_len()..];
                    }

                    emit_ip(&ip_repr, tx_buffer);
                    Ok(())
                })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum DispatchError {
    MtuRetry(usize),
    /// The selected transmit backend has no capacity. The socket should retain
    /// the packet and retry after the device is polled again.
    Exhausted,
    /// The integration's LOCAL_OUT policy denied the packet.
    PolicyDrop,
    /// No route to dispatch this packet. Retrying won't help unless
    /// configuration is changed.
    NoRoute,
    /// We do have a route to dispatch this packet, but we haven't discovered
    /// the neighbor for it yet. Discovery has been initiated, dispatch
    /// should be retried later.
    NeighborPending,
}
