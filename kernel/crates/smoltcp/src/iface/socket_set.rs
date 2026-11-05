use core::fmt;
#[cfg(all(feature = "alloc", feature = "socket-tcp"))]
#[path = "tcp_time_wait.rs"]
mod tcp_time_wait;
use crate::storage::BufferStorage;
use managed::ManagedSlice;

#[cfg(all(
    feature = "alloc",
    any(feature = "socket-udp", feature = "socket-tcp"),
    not(target_has_atomic = "ptr")
))]
use alloc::rc::Rc as Shared;
#[cfg(all(
    feature = "alloc",
    any(feature = "socket-udp", feature = "socket-tcp"),
    target_has_atomic = "ptr"
))]
use alloc::sync::Arc as Shared;

use super::socket_meta::Meta;
#[cfg(all(feature = "alloc", any(feature = "socket-udp", feature = "socket-tcp")))]
use crate::phy::PacketMeta;
use crate::socket::{AnySocket, Socket};
#[cfg(all(feature = "alloc", feature = "socket-tcp"))]
use crate::wire::IpEndpoint;
#[cfg(all(feature = "alloc", any(feature = "socket-udp", feature = "socket-tcp")))]
use crate::wire::IpRepr;
#[cfg(all(feature = "alloc", feature = "socket-udp"))]
use crate::wire::UdpRepr;

/// Result of offering a validated TCP segment to an external consumer.
#[cfg(all(feature = "alloc", feature = "socket-tcp"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpIngressResult {
    /// Continue normal TCP socket and TIME_WAIT demultiplexing.
    NotHandled,
    /// Consumed or deliberately dropped; do not generate a local reset.
    Consumed,
}

/// Optional TCP consumer, called after IP reassembly and TCP validation.
///
/// The segment includes the TCP header. The IP representation describes this
/// segment without extension headers. Metadata identifies the actual ingress
/// device. Implementations must not reenter this interface or socket set.
#[cfg(all(feature = "alloc", feature = "socket-tcp"))]
pub trait TcpIngressHandler: fmt::Debug + Send + Sync {
    fn handle_tcp_ingress(
        &self,
        meta: PacketMeta,
        ip_repr: &IpRepr,
        segment: &[u8],
    ) -> TcpIngressResult;
}

/// Result of offering a validated UDP datagram to an external ingress handler.
#[cfg(all(feature = "alloc", feature = "socket-udp"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpIngressResult {
    /// Continue with smoltcp's UDP and DNS socket demultiplexing.
    NotHandled,
    /// The handler consumed or deliberately dropped the datagram.
    ///
    /// smoltcp will not offer it to UDP/DNS sockets and will not emit an ICMP
    /// port-unreachable response.
    Consumed,
}

/// Optional external consumer for validated UDP ingress.
///
/// The callback runs after IP reassembly and UDP length/checksum validation,
/// but before smoltcp UDP and DNS socket demultiplexing.
/// `is_broadcast` reports limited or interface-directed IPv4 broadcast; it is
/// always false for IPv6.
#[cfg(all(feature = "alloc", feature = "socket-udp"))]
pub trait UdpIngressHandler: fmt::Debug + Send + Sync {
    fn handle_udp_ingress(
        &self,
        meta: PacketMeta,
        ip_repr: &IpRepr,
        udp_repr: &UdpRepr,
        is_broadcast: bool,
        payload: &[u8],
    ) -> UdpIngressResult;
}

/// Optional registry of logical TCP listeners whose socket slots may be exhausted.
///
/// Queried only for a validated new SYN that no TCP socket accepts. Returning true
/// suppresses the closed-port reset so that the peer can retry when a slot is free.
/// Implementations must reflect listener removal and must not reenter the socket
/// set or acquire locks held by its caller.
#[cfg(all(feature = "alloc", feature = "socket-tcp"))]
pub trait TcpListenRegistry: fmt::Debug + Send + Sync {
    /// Match the local endpoint and the ingress device in packet metadata.
    fn is_listening(&self, local_endpoint: IpEndpoint, meta: crate::phy::PacketMeta) -> bool;
}

/// Opaque struct with space for storing one socket.
///
/// This is public so you can use it to allocate space for storing
/// sockets when creating an Interface.
#[derive(Debug, Default)]
pub struct SocketStorage<'a> {
    inner: Option<Item<'a>>,
    reserved: bool,
}

/// A prepared allocation of empty slots, or retired storage returned by a
/// replacement. Its owner releases any allocation receipt only on real free.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub struct SocketSetStorage<'storage, 'a: 'storage> {
    slots: BufferStorage<'storage, SocketStorage<'a>>,
}

#[cfg(feature = "alloc")]
impl<'storage, 'a: 'storage> SocketSetStorage<'storage, 'a> {
    pub fn from_owned(
        storage: alloc::boxed::Box<dyn crate::storage::OwnedBuffer<SocketStorage<'a>> + 'storage>,
    ) -> Self {
        Self {
            slots: BufferStorage::Owned(storage),
        }
    }
}

impl<'a> SocketStorage<'a> {
    pub const EMPTY: Self = Self {
        inner: None,
        reserved: false,
    };
}

/// An item of a socket set.
#[derive(Debug)]
pub struct Item<'a> {
    pub meta: Meta,
    pub socket: Socket<'a>,
}

/// A handle, identifying a socket in an Interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct SocketHandle(usize);

impl fmt::Display for SocketHandle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// An extensible set of sockets.
///
/// The lifetime `'storage` describes the slot array, while `'a` describes the
/// buffers of the stored `Socket<'a>` values. Keeping them independent allows
/// borrowed slot arrays to contain sockets with owned (`'static`) buffers.
/// Both lifetimes can be `'static` when slots and socket buffers are owned.
#[derive(Debug)]
pub struct SocketSet<'storage, 'a: 'storage> {
    sockets: BufferStorage<'storage, SocketStorage<'a>>,
    #[cfg(all(feature = "alloc", feature = "socket-tcp"))]
    time_wait: tcp_time_wait::TimeWaitTable,
    #[cfg(all(feature = "alloc", feature = "socket-tcp"))]
    tcp_listen_registry: Option<Shared<dyn TcpListenRegistry>>,
    #[cfg(all(feature = "alloc", feature = "socket-tcp"))]
    tcp_ingress_handler: Option<Shared<dyn TcpIngressHandler>>,
    #[cfg(all(feature = "alloc", feature = "socket-udp"))]
    udp_ingress_handler: Option<Shared<dyn UdpIngressHandler>>,
}

impl<'storage, 'a: 'storage> SocketSet<'storage, 'a> {
    /// Total initialized slots, including empty reusable slots.
    pub fn storage_slots(&self) -> usize {
        self.sockets.len()
    }

    pub fn available_slots(&self) -> usize {
        self.sockets
            .iter()
            .filter(|slot| slot.inner.is_none() && !slot.reserved)
            .count()
    }

    /// Swap an empty prepared array, preserving every existing handle/index.
    /// Does not allocate or drop any socket/backing allocation. On failure
    /// the candidate is returned intact; on success retire old storage unlocked.
    #[cfg(feature = "alloc")]
    pub fn replace_prepared_storage(
        &mut self,
        mut candidate: SocketSetStorage<'storage, 'a>,
    ) -> Result<SocketSetStorage<'storage, 'a>, SocketSetStorage<'storage, 'a>> {
        if candidate.slots.len() < self.sockets.len()
            || candidate
                .slots
                .iter()
                .any(|slot| slot.inner.is_some() || slot.reserved)
        {
            return Err(candidate);
        }
        for (old, new) in self.sockets.iter_mut().zip(candidate.slots.iter_mut()) {
            *new = core::mem::replace(old, SocketStorage::EMPTY);
        }
        core::mem::swap(&mut self.sockets, &mut candidate.slots);
        Ok(candidate)
    }

    /// An integration with explicit allocation admission never permits an
    /// implicit Vec grow while holding its SocketSet lock.
    pub fn try_add_no_grow<T: AnySocket<'a>>(&mut self, socket: T) -> Result<SocketHandle, T> {
        let Some(index) = self
            .sockets
            .iter()
            .position(|slot| slot.inner.is_none() && !slot.reserved)
        else {
            return Err(socket);
        };
        let handle = SocketHandle(index);
        let mut meta = Meta::default();
        meta.handle = handle;
        self.sockets[index].inner = Some(Item {
            meta,
            socket: socket.upcast(),
        });
        Ok(handle)
    }

    /// Reserve an existing vacancy without allocation. As with every socket
    /// handle, this handle belongs exclusively to this set. It must be consumed
    /// once with add_reserved or cancel_reserved; ordinary add skips it.
    pub fn reserve_slot_no_grow(&mut self) -> Option<SocketHandle> {
        let index = self
            .sockets
            .iter()
            .position(|slot| slot.inner.is_none() && !slot.reserved)?;
        self.sockets[index].reserved = true;
        Some(SocketHandle(index))
    }

    pub fn cancel_reserved(&mut self, handle: SocketHandle) {
        let slot = &mut self.sockets[handle.0];
        assert!(
            slot.reserved && slot.inner.is_none(),
            "handle is not a reserved vacancy"
        );
        slot.reserved = false;
    }

    /// Pin an occupied slot for a move-and-return operation. Removing its
    /// socket preserves this mark, so no ordinary insertion can steal it.
    pub fn reserve_existing(&mut self, handle: SocketHandle) {
        let slot = &mut self.sockets[handle.0];
        assert!(slot.inner.is_some() && !slot.reserved);
        slot.reserved = true;
    }

    /// Cancel before removal; unlike cancel_reserved the socket is still live.
    pub fn unreserve_existing(&mut self, handle: SocketHandle) {
        let slot = &mut self.sockets[handle.0];
        assert!(slot.inner.is_some() && slot.reserved);
        slot.reserved = false;
    }

    /// Commit a previously reserved vacancy. The caller's lease guarantees
    /// this slot is not reused; no storage preparation remains at this point.
    pub fn add_reserved<T: AnySocket<'a>>(
        &mut self,
        handle: SocketHandle,
        socket: T,
    ) -> SocketHandle {
        self.add_reserved_socket(handle, socket.upcast())
    }

    /// Move an already erased protocol without rebuilding its owned storage.
    pub fn add_reserved_socket(
        &mut self,
        handle: SocketHandle,
        socket: Socket<'a>,
    ) -> SocketHandle {
        let slot = &mut self.sockets[handle.0];
        assert!(
            slot.reserved && slot.inner.is_none(),
            "handle is not a reserved vacancy"
        );
        let mut meta = Meta::default();
        meta.handle = handle;
        slot.inner = Some(Item { meta, socket });
        slot.reserved = false;
        handle
    }

    /// Create a socket set using the provided storage.
    pub fn new<SocketsT>(sockets: SocketsT) -> SocketSet<'storage, 'a>
    where
        SocketsT: Into<ManagedSlice<'storage, SocketStorage<'a>>>,
    {
        let sockets = sockets.into();
        SocketSet {
            sockets: BufferStorage::Managed(sockets),
            #[cfg(all(feature = "alloc", feature = "socket-tcp"))]
            time_wait: tcp_time_wait::TimeWaitTable::default(),
            #[cfg(all(feature = "alloc", feature = "socket-tcp"))]
            tcp_listen_registry: None,
            #[cfg(all(feature = "alloc", feature = "socket-tcp"))]
            tcp_ingress_handler: None,
            #[cfg(all(feature = "alloc", feature = "socket-udp"))]
            udp_ingress_handler: None,
        }
    }

    /// Install or remove the external TCP ingress handler.
    #[cfg(all(feature = "alloc", feature = "socket-tcp"))]
    pub fn set_tcp_ingress_handler(
        &mut self,
        handler: Option<Shared<dyn TcpIngressHandler>>,
    ) -> Option<Shared<dyn TcpIngressHandler>> {
        core::mem::replace(&mut self.tcp_ingress_handler, handler)
    }

    #[cfg(all(feature = "alloc", feature = "socket-tcp"))]
    pub(crate) fn handle_tcp_ingress(
        &self,
        meta: PacketMeta,
        ip_repr: &IpRepr,
        segment: &[u8],
    ) -> TcpIngressResult {
        self.tcp_ingress_handler
            .as_ref()
            .map_or(TcpIngressResult::NotHandled, |handler| {
                handler.handle_tcp_ingress(meta, ip_repr, segment)
            })
    }

    /// Install or remove the external TCP listener registry, returning the previous one.
    #[cfg(all(feature = "alloc", feature = "socket-tcp"))]
    pub fn set_tcp_listen_registry(
        &mut self,
        registry: Option<Shared<dyn TcpListenRegistry>>,
    ) -> Option<Shared<dyn TcpListenRegistry>> {
        core::mem::replace(&mut self.tcp_listen_registry, registry)
    }

    #[cfg(all(feature = "alloc", feature = "socket-tcp"))]
    pub(crate) fn tcp_is_listening(
        &self,
        endpoint: IpEndpoint,
        meta: crate::phy::PacketMeta,
    ) -> bool {
        self.tcp_listen_registry
            .as_ref()
            .map_or(false, |registry| registry.is_listening(endpoint, meta))
    }

    /// Install or remove the external UDP ingress handler.
    #[cfg(all(feature = "alloc", feature = "socket-udp"))]
    pub fn set_udp_ingress_handler(
        &mut self,
        handler: Option<Shared<dyn UdpIngressHandler>>,
    ) -> Option<Shared<dyn UdpIngressHandler>> {
        core::mem::replace(&mut self.udp_ingress_handler, handler)
    }

    #[cfg(all(feature = "alloc", feature = "socket-udp"))]
    pub(crate) fn handle_udp_ingress(
        &self,
        meta: PacketMeta,
        ip_repr: &IpRepr,
        udp_repr: &UdpRepr,
        is_broadcast: bool,
        payload: &[u8],
    ) -> UdpIngressResult {
        self.udp_ingress_handler
            .as_ref()
            .map_or(UdpIngressResult::NotHandled, |handler| {
                handler.handle_udp_ingress(meta, ip_repr, udp_repr, is_broadcast, payload)
            })
    }

    /// Add a socket to the set, and return its handle.
    ///
    /// # Panics
    /// This function panics if the storage is fixed-size (not a `Vec`) and is full.
    pub fn add<T: AnySocket<'a>>(&mut self, socket: T) -> SocketHandle {
        fn put<'a>(index: usize, slot: &mut SocketStorage<'a>, socket: Socket<'a>) -> SocketHandle {
            net_trace!("[{}]: adding", index);
            let handle = SocketHandle(index);
            let mut meta = Meta::default();
            meta.handle = handle;
            *slot = SocketStorage {
                inner: Some(Item { meta, socket }),
                reserved: false,
            };
            handle
        }

        let socket = socket.upcast();

        for (index, slot) in self.sockets.iter_mut().enumerate() {
            if slot.inner.is_none() && !slot.reserved {
                return put(index, slot, socket);
            }
        }

        match &mut self.sockets {
            BufferStorage::Managed(ManagedSlice::Borrowed(_)) => {
                panic!("adding a socket to a full SocketSet")
            }
            #[cfg(feature = "alloc")]
            BufferStorage::Managed(ManagedSlice::Owned(sockets)) => {
                sockets.push(SocketStorage::EMPTY);
                let index = sockets.len() - 1;
                put(index, &mut sockets[index], socket)
            }
            #[cfg(feature = "alloc")]
            BufferStorage::Owned(_) => {
                panic!("adding a socket requires prepared SocketSet capacity")
            }
        }
    }

    /// Get a socket from the set by its handle, as mutable.
    ///
    /// # Panics
    /// This function may panic if the handle does not belong to this socket set
    /// or the socket has the wrong type.
    pub fn get<T: AnySocket<'a>>(&self, handle: SocketHandle) -> &T {
        match self.sockets[handle.0].inner.as_ref() {
            Some(item) => {
                T::downcast(&item.socket).expect("handle refers to a socket of a wrong type")
            }
            None => panic!("handle does not refer to a valid socket"),
        }
    }

    /// Get a mutable socket from the set by its handle, as mutable.
    ///
    /// # Panics
    /// This function may panic if the handle does not belong to this socket set
    /// or the socket has the wrong type.
    pub fn get_mut<T: AnySocket<'a>>(&mut self, handle: SocketHandle) -> &mut T {
        match self.sockets[handle.0].inner.as_mut() {
            Some(item) => T::downcast_mut(&mut item.socket)
                .expect("handle refers to a socket of a wrong type"),
            None => panic!("handle does not refer to a valid socket"),
        }
    }

    /// Remove a socket from the set, without changing its state.
    ///
    /// # Panics
    /// This function may panic if the handle does not belong to this socket set.
    pub fn remove(&mut self, handle: SocketHandle) -> Socket<'a> {
        net_trace!("[{}]: removing", handle.0);
        match self.sockets[handle.0].inner.take() {
            Some(item) => item.socket,
            None => panic!("handle does not refer to a valid socket"),
        }
    }

    /// Get an iterator to the inner sockets.
    pub fn iter(&self) -> impl Iterator<Item = (SocketHandle, &Socket<'a>)> {
        self.items().map(|i| (i.meta.handle, &i.socket))
    }

    /// Get a mutable iterator to the inner sockets.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (SocketHandle, &mut Socket<'a>)> {
        self.items_mut().map(|i| (i.meta.handle, &mut i.socket))
    }

    /// Iterate every socket in this set.
    pub fn items(&self) -> impl Iterator<Item = &Item<'a>> + '_ {
        self.sockets.iter().filter_map(|x| x.inner.as_ref())
    }

    /// Iterate every socket in this set.
    pub fn items_mut(&mut self) -> impl Iterator<Item = &mut Item<'a>> + '_ {
        self.sockets.iter_mut().filter_map(|x| x.inner.as_mut())
    }
}
