/*! Access to networking hardware.

The `phy` module deals with the *network devices*. It provides a trait
for transmitting and receiving frames, [Device](trait.Device.html)
and implementations of it:

  * the [_loopback_](struct.Loopback.html), for zero dependency testing;
  * _middleware_ [Tracer](struct.Tracer.html) and
    [FaultInjector](struct.FaultInjector.html), to facilitate debugging;
  * _adapters_ [RawSocket](struct.RawSocket.html) and
    [TunTapInterface](struct.TunTapInterface.html), to transmit and receive frames
    on the host OS.
*/
#![cfg_attr(
    feature = "medium-ethernet",
    doc = r##"
# Examples

An implementation of the [Device](trait.Device.html) trait for a simple hardware
Ethernet controller could look as follows:

```rust
use smoltcp::phy::{self, DeviceCapabilities, Device, Medium};
use smoltcp::time::Instant;

struct StmPhy {
    rx_buffer: [u8; 1536],
    tx_buffer: [u8; 1536],
}

impl<'a> StmPhy {
    fn new() -> StmPhy {
        StmPhy {
            rx_buffer: [0; 1536],
            tx_buffer: [0; 1536],
        }
    }
}

impl phy::Device for StmPhy {
    type RxToken<'a> = StmPhyRxToken<'a> where Self: 'a;
    type TxToken<'a> = StmPhyTxToken<'a> where Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        Some((StmPhyRxToken(&mut self.rx_buffer[..]),
              StmPhyTxToken(&mut self.tx_buffer[..])))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(StmPhyTxToken(&mut self.tx_buffer[..]))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1536;
        caps.max_burst_size = Some(1);
        caps.medium = Medium::Ethernet;
        caps
    }
}

struct StmPhyRxToken<'a>(&'a mut [u8]);

impl<'a> phy::RxToken for StmPhyRxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
        where F: FnOnce(& [u8]) -> R
    {
        // TODO: receive packet into buffer
        let result = f(&self.0);
        println!("rx called");
        result
    }
}

struct StmPhyTxToken<'a>(&'a mut [u8]);

impl<'a> phy::TxToken for StmPhyTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
        where F: FnOnce(&mut [u8]) -> R
    {
        let result = f(&mut self.0[..len]);
        println!("tx called {}", len);
        // TODO: send packet out
        result
    }
}
```
"##
)]

use crate::time::Instant;

#[cfg(all(
    any(feature = "phy-raw_socket", feature = "phy-tuntap_interface"),
    unix
))]
mod sys;

mod fault_injector;
#[cfg(feature = "alloc")]
mod fuzz_injector;
#[cfg(feature = "alloc")]
mod loopback;
mod pcap_writer;
#[cfg(all(feature = "phy-raw_socket", unix))]
mod raw_socket;
mod tracer;
#[cfg(all(
    feature = "phy-tuntap_interface",
    any(target_os = "linux", target_os = "android")
))]
mod tuntap_interface;

#[cfg(all(
    any(feature = "phy-raw_socket", feature = "phy-tuntap_interface"),
    unix
))]
pub use self::sys::wait;

pub use self::fault_injector::FaultInjector;
#[cfg(feature = "alloc")]
pub use self::fuzz_injector::{FuzzInjector, Fuzzer};
#[cfg(feature = "alloc")]
pub use self::loopback::Loopback;
pub use self::pcap_writer::{PcapLinkType, PcapMode, PcapSink, PcapWriter};
#[cfg(all(feature = "phy-raw_socket", unix))]
pub use self::raw_socket::RawSocket;
pub use self::tracer::Tracer;
#[cfg(all(
    feature = "phy-tuntap_interface",
    any(target_os = "linux", target_os = "android")
))]
pub use self::tuntap_interface::TunTapInterface;

/// Metadata associated to a packet.
///
/// The packet metadata is a set of attributes associated to network packets
/// as they travel up or down the stack. The metadata is get/set by the
/// [`Device`] implementations or by the user when sending/receiving packets from a
/// socket.
///
/// Metadata fields are enabled via Cargo features. If no field is enabled, this
/// struct becomes zero-sized, which allows the compiler to optimize it out as if
/// the packet metadata mechanism didn't exist at all.
///
/// UDP sockets allow setting/retrieving packet metadata. TCP sockets can constrain
/// their ingress and egress device using the `packetmeta-id` field. Other sockets
/// emit packets with default metadata.
///
/// This struct is marked as `#[non_exhaustive]`. This means it is not possible to
/// create it directly by specifying all fields. You have to instead create it with
/// default values and then set the fields you want. This makes adding metadata
/// fields a non-breaking change.
///
/// ```rust
/// let mut meta = smoltcp::phy::PacketMeta::default();
/// #[cfg(feature = "packetmeta-id")]
/// {
///     meta.id = 15;
/// }
/// ```
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, Default)]
#[non_exhaustive]
pub struct PacketMeta {
    #[cfg(feature = "packetmeta-id")]
    pub id: u32,
}

/// A description of checksum behavior for a particular protocol.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Checksum {
    /// Verify checksum when receiving and compute checksum when sending.
    #[default]
    Both,
    /// Verify checksum when receiving.
    Rx,
    /// Compute checksum before sending.
    Tx,
    /// Ignore checksum completely.
    None,
}

impl Checksum {
    /// Returns whether checksum should be verified when receiving.
    pub fn rx(&self) -> bool {
        match *self {
            Checksum::Both | Checksum::Rx => true,
            _ => false,
        }
    }

    /// Returns whether checksum should be verified when sending.
    pub fn tx(&self) -> bool {
        match *self {
            Checksum::Both | Checksum::Tx => true,
            _ => false,
        }
    }
}

/// A description of checksum behavior for every supported protocol.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub struct ChecksumCapabilities {
    pub ipv4: Checksum,
    pub udp: Checksum,
    pub tcp: Checksum,
    #[cfg(feature = "proto-ipv4")]
    pub icmpv4: Checksum,
    #[cfg(feature = "proto-ipv6")]
    pub icmpv6: Checksum,
}

impl ChecksumCapabilities {
    /// Checksum behavior that results in not computing or verifying checksums
    /// for any of the supported protocols.
    pub fn ignored() -> Self {
        ChecksumCapabilities {
            ipv4: Checksum::None,
            udp: Checksum::None,
            tcp: Checksum::None,
            #[cfg(feature = "proto-ipv4")]
            icmpv4: Checksum::None,
            #[cfg(feature = "proto-ipv6")]
            icmpv6: Checksum::None,
        }
    }
}

/// A description of device capabilities.
///
/// Higher-level protocols may achieve higher throughput or lower latency if they consider
/// the bandwidth or packet size limitations.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub struct DeviceCapabilities {
    /// Medium of the device.
    ///
    /// This indicates what kind of packet the sent/received bytes are, and determines
    /// some behaviors of Interface. For example, ARP/NDISC address resolution is only done
    /// for Ethernet mediums.
    pub medium: Medium,

    /// Maximum transmission unit.
    ///
    /// The network device is unable to send or receive frames larger than the value returned
    /// by this function.
    ///
    /// For Ethernet devices, this is the maximum Ethernet frame size, including the Ethernet header (14 octets), but
    /// *not* including the Ethernet FCS (4 octets). Therefore, Ethernet MTU = IP MTU + 14.
    ///
    /// Note that in Linux and other OSes, "MTU" is the IP MTU, not the Ethernet MTU, even for Ethernet
    /// devices. This is a common source of confusion.
    ///
    /// Most common IP MTU is 1500. Minimum is 576 (for IPv4) or 1280 (for IPv6). Maximum is 9216 octets.
    pub max_transmission_unit: usize,

    /// Maximum burst size, in terms of MTU.
    ///
    /// The network device is unable to send or receive bursts large than the value returned
    /// by this function.
    ///
    /// If `None`, there is no fixed limit on burst size, e.g. if network buffers are
    /// dynamically allocated.
    pub max_burst_size: Option<usize>,

    /// Checksum behavior.
    ///
    /// If the network device is capable of verifying or computing checksums for some protocols,
    /// it can request that the stack not do so in software to improve performance.
    pub checksum: ChecksumCapabilities,
}

impl DeviceCapabilities {
    pub fn ip_mtu(&self) -> usize {
        match self.medium {
            #[cfg(feature = "medium-ethernet")]
            Medium::Ethernet => {
                self.max_transmission_unit - crate::wire::EthernetFrame::<&[u8]>::header_len()
            }
            #[cfg(feature = "medium-ip")]
            Medium::Ip => self.max_transmission_unit,
            #[cfg(feature = "medium-ieee802154")]
            Medium::Ieee802154 => self.max_transmission_unit, // TODO(thvdveld): what is the MTU for Medium::IEEE802
        }
    }
}

/// Type of medium of a device.
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Medium {
    /// Ethernet medium. Devices of this type send and receive Ethernet frames,
    /// and interfaces using it must do neighbor discovery via ARP or NDISC.
    ///
    /// Examples of devices of this type are Ethernet, WiFi (802.11), Linux `tap`, and VPNs in tap (layer 2) mode.
    #[cfg(feature = "medium-ethernet")]
    Ethernet,

    /// IP medium. Devices of this type send and receive IP frames, without an
    /// Ethernet header. MAC addresses are not used, and no neighbor discovery (ARP, NDISC) is done.
    ///
    /// Examples of devices of this type are the Linux `tun`, PPP interfaces, VPNs in tun (layer 3) mode.
    #[cfg(feature = "medium-ip")]
    Ip,

    #[cfg(feature = "medium-ieee802154")]
    Ieee802154,
}

impl Default for Medium {
    fn default() -> Medium {
        #[cfg(feature = "medium-ethernet")]
        return Medium::Ethernet;
        #[cfg(all(feature = "medium-ip", not(feature = "medium-ethernet")))]
        return Medium::Ip;
        #[cfg(all(
            feature = "medium-ieee802154",
            not(feature = "medium-ip"),
            not(feature = "medium-ethernet")
        ))]
        return Medium::Ieee802154;
        #[cfg(all(
            not(feature = "medium-ip"),
            not(feature = "medium-ethernet"),
            not(feature = "medium-ieee802154")
        ))]
        return panic!("No medium enabled");
    }
}

/// An interface for sending and receiving raw network frames.
///
/// The interface is based on _tokens_, which are types that allow to receive/transmit a
/// single packet. The `receive` and `transmit` functions only construct such tokens, the
/// real sending/receiving operation are performed when the tokens are consumed.
pub trait Device {
    type RxToken<'a>: RxToken
    where
        Self: 'a;
    type TxToken<'a>: TxToken
    where
        Self: 'a;

    /// Construct a token pair consisting of one receive token and one transmit token.
    ///
    /// The additional transmit token makes it possible to generate a reply packet based
    /// on the contents of the received packet. For example, this makes it possible to
    /// handle arbitrarily large ICMP echo ("ping") requests, where the all received bytes
    /// need to be sent back, without heap allocation.
    ///
    /// The timestamp must be a number of milliseconds, monotonically increasing since an
    /// arbitrary moment in time, such as system startup.
    fn receive(&mut self, timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)>;

    /// Construct a transmit token.
    ///
    /// The timestamp must be a number of milliseconds, monotonically increasing since an
    /// arbitrary moment in time, such as system startup.
    fn transmit(&mut self, timestamp: Instant) -> Option<Self::TxToken<'_>>;

    /// Get a description of device capabilities.
    fn capabilities(&self) -> DeviceCapabilities;

    /// Read the effective IP MTU without reserving a transmit token.
    /// Integrations with an external FIB must use the same route snapshot as
    /// `TxToken::egress_override`. An unavailable route should return the
    /// native MTU so TCP can still advance its timers; transmission may fail.
    fn outbound_ip_mtu(&self, _destination: crate::wire::IpAddress, _meta: PacketMeta) -> usize {
        self.capabilities().ip_mtu()
    }

    /// TCP route lookup may require the original four-tuple to resolve NAT.
    fn outbound_tcp_mtu(
        &self,
        _local: crate::wire::IpEndpoint,
        remote: crate::wire::IpEndpoint,
        meta: PacketMeta,
    ) -> usize {
        self.outbound_ip_mtu(remote.addr, meta)
    }

    /// Invalidate local MTU retry hints when routing or output policy changes.
    /// Integrations which return MTU retries must change this value whenever
    /// the path selected for a TCP flow may change.
    fn outbound_tcp_mtu_generation(&self) -> u64 {
        0
    }

    /// Whether the integration's policy view is still current. A poll may
    /// dispatch several distinct packets; stop before advancing the next
    /// socket when its pinned policy has been replaced.
    fn policy_current(&self) -> bool {
        true
    }
}

/// A token to receive a single network packet.
pub trait RxToken {
    /// Consumes the token to receive a single network packet.
    ///
    /// This method receives a packet and then calls the given closure `f` with the raw
    /// packet bytes as argument.
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R;

    /// The Packet ID associated with the frame received by this [`RxToken`]
    fn meta(&self) -> PacketMeta {
        PacketMeta::default()
    }
}

/// A token to transmit a single network packet.
pub trait TxToken {
    /// Prepare the exact frame length before serialization or fragment progress
    /// is committed. Repeating a successful preparation for the same length
    /// must not acquire another reservation. Failure must leave emission and
    /// protocol progress untouched. Existing infallible devices need no hook.
    fn prepare_buffer(&mut self, _len: usize) -> Result<(), TxEgressError> {
        Ok(())
    }

    /// Whether this token hands a complete, serialized IP packet to its
    /// integration before smoltcp performs neighbor lookup or fragmentation.
    /// The default leaves all output ownership with smoltcp.
    fn deferred_ip_output(&self, _version: crate::wire::IpVersion) -> bool {
        false
    }

    /// Atomically admit a complete IP packet to an integration-owned output
    /// queue. Called only when [`TxToken::deferred_ip_output`] is true. The
    /// closure must fill the entire buffer while socket payload borrows are
    /// still valid; admission may reject the packet without committing it.
    fn consume_full_ip<F>(
        self,
        len: usize,
        meta: PacketMeta,
        class: IpOutputClass,
        ipv4_fragment_ident: Option<u16>,
        emit: F,
    ) -> Result<(), IpOutputError>
    where
        F: FnOnce(&mut [u8]),
        Self: Sized,
    {
        let _ = (self, len, meta, class, ipv4_fragment_ident, emit);
        Err(IpOutputError::Unsupported)
    }

    /// Override the egress properties used to serialize this packet.
    ///
    /// Most devices use the interface medium and return `None`. Stack
    /// integrations that defer routing and link-layer resolution may request
    /// an IP packet and the route-selected IP MTU even when the interface
    /// itself uses Ethernet.
    fn egress_override(
        &mut self,
        version: crate::wire::IpVersion,
        destination: crate::wire::IpAddress,
        meta: PacketMeta,
    ) -> Result<Option<TxEgressOverride>, TxEgressError> {
        let _ = (version, destination, meta);
        Ok(None)
    }

    /// Re-admit the backend selected while an earlier fragment was emitted.
    ///
    /// The default implementation is a no-op. Integrations that attach an
    /// This is called for every continuation fragment. `None` identifies the
    /// device's native egress path; integrations that attach an opaque routing
    /// decision to [`TxEgressOverride`] can restore it from `Some`.
    /// Return [`TxEgressError::Exhausted`] when the token cannot currently
    /// honor the saved properties. The stack retains the remaining fragments
    /// for a later poll without calling `consume`.
    fn apply_egress_override(
        &mut self,
        egress: Option<TxEgressOverride>,
    ) -> Result<(), TxEgressError> {
        let _ = egress;
        Ok(())
    }

    /// Consumes the token to send a single network packet.
    ///
    /// This method constructs a transmit buffer of size `len` and calls the passed
    /// closure `f` with a mutable reference to that buffer. The closure should construct
    /// a valid network packet (e.g. an ethernet packet) in the buffer. When the closure
    /// returns, the transmit buffer is sent out.
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R;

    /// The Packet ID to be associated with the frame to be transmitted by this [`TxToken`].
    #[allow(unused_variables)]
    fn set_meta(&mut self, meta: PacketMeta) {}
}

/// Routing scope for an integration-owned, locally generated IP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum IpOutputClass {
    Ordinary,
    /// IPv6 neighbor discovery must use the current link, not a namespace
    /// route, but it still traverses the IPv6 LOCAL_OUT policy.
    LinkLocalControl,
}

/// Failure to admit a complete IP packet. Policy denial is distinct from
/// transient device capacity and from route/MTU failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum IpOutputError {
    Unsupported,
    Exhausted,
    NoRoute,
    PolicyDrop,
    MtuExceeded,
    /// The final output route requires TCP to construct a smaller segment.
    /// The packet was not admitted; output hooks may already have run.
    MtuRetry(usize),
}

/// A transmit backend could not admit the packet selected by
/// [`TxToken::egress_override`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum TxEgressError {
    Exhausted,
}

/// Route-selected properties for a packet emitted through a transmit token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxEgressOverride {
    /// Medium used to serialize the packet handed to the token.
    pub medium: Medium,
    /// Maximum IP packet size accepted by the selected egress route.
    pub ip_mtu: usize,
    /// Opaque integration-defined context preserved across IP fragments.
    pub context: [u64; 3],
}

#[cfg(all(test, feature = "std", feature = "medium-ip"))]
mod preparation_tests {
    use super::*;
    use core::cell::Cell;
    use std::rc::Rc;
    struct Rx;
    impl RxToken for Rx {
        fn consume<R, F>(self, _: F) -> R
        where
            F: FnOnce(&[u8]) -> R,
        {
            unreachable!()
        }
    }
    struct Tx(Rc<Cell<usize>>);
    impl TxToken for Tx {
        fn prepare_buffer(&mut self, len: usize) -> Result<(), TxEgressError> {
            assert_eq!(len, 42);
            self.0.set(self.0.get() + 1);
            Err(TxEgressError::Exhausted)
        }
        fn consume<R, F>(self, _: usize, _: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            panic!("not prepared")
        }
    }
    struct Dev(Rc<Cell<usize>>);
    impl Device for Dev {
        type RxToken<'a> = Rx;
        type TxToken<'a> = Tx;
        fn receive(&mut self, _: Instant) -> Option<(Rx, Tx)> {
            None
        }
        fn transmit(&mut self, _: Instant) -> Option<Tx> {
            Some(Tx(self.0.clone()))
        }
        fn capabilities(&self) -> DeviceCapabilities {
            DeviceCapabilities {
                medium: Medium::Ip,
                max_transmission_unit: 1500,
                ..DeviceCapabilities::default()
            }
        }
    }
    struct NoFuzz;
    impl Fuzzer for NoFuzz {
        fn fuzz_packet(&self, _: &mut [u8]) {
            panic!("prepare must not fuzz")
        }
    }
    struct Sink(Rc<Cell<usize>>);
    impl PcapSink for Sink {
        fn write(&mut self, _: &[u8]) {
            self.0.set(self.0.get() + 1);
        }
    }
    #[test]
    fn all_middleware_forward_prepare_without_emission_side_effects() {
        let calls = Rc::new(Cell::new(0));
        let writes = Rc::new(Cell::new(0));
        let fuzz = FuzzInjector::new(Dev(calls.clone()), NoFuzz, NoFuzz);
        let pcap = PcapWriter::new(fuzz, Sink(writes.clone()), PcapMode::Both);
        let tracer = Tracer::new(pcap, |_, _| panic!("prepare must not trace"));
        let mut fault = FaultInjector::new(tracer, 123);
        let headers = writes.get();
        let mut tx = fault.transmit(Instant::from_millis(0)).unwrap();
        for _ in 0..2 {
            assert_eq!(tx.prepare_buffer(42), Err(TxEgressError::Exhausted));
        }
        assert_eq!(calls.get(), 2);
        assert_eq!(writes.get(), headers);
    }
}
