use smoltcp::wire::{EthernetAddress, IpAddress};

pub(crate) const NTF_ROUTER: u8 = 0x80;

pub(crate) const NUD_FAILED: u16 = 0x20;
pub(crate) const NUD_NOARP: u16 = 0x40;
pub(crate) const NUD_PERMANENT: u16 = 0x80;

pub(crate) const RTN_UNICAST: u8 = 1;

/// Linux-visible identity and configured state for a non-aging L3 neighbor.
///
/// The address family is carried by `destination`; keeping it out of the key
/// prevents a second field from drifting from the actual address.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct NeighborEntry {
    pub(crate) ifindex: u32,
    pub(crate) destination: IpAddress,
    /// Headerless devices keep this empty rather than fabricating Ethernet.
    pub(crate) lladdr: Option<EthernetAddress>,
    /// Whether the entry is eligible to resolve an Ethernet next hop.
    pub(crate) ethernet_output: bool,
    pub(crate) state: u16,
    pub(crate) flags: u8,
    pub(crate) kind: u8,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct NeighborUpdate {
    pub(crate) ifindex: u32,
    pub(crate) destination: IpAddress,
    pub(crate) lladdr: Option<EthernetAddress>,
    pub(crate) state: u16,
    pub(crate) flags: u8,
    pub(crate) protocol: u8,
    pub(crate) flags_ext: u32,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NeighborNewFlags {
    pub(crate) replace: bool,
    pub(crate) excl: bool,
    pub(crate) create: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum NeighborMutationOutcome {
    Added(NeighborEntry),
    Updated {
        old: NeighborEntry,
        new: NeighborEntry,
    },
    Unchanged(NeighborEntry),
}
