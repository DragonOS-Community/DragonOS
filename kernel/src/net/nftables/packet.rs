use super::*;

pub(super) type RedirectAddressResolver<'a> = dyn Fn(&[u8]) -> Option<CtAddress> + 'a;

/// The packet path supplies its already-acquired routing view. Route-aware
/// expressions must not acquire the FIB behind a smoltcp interface lock.
pub(crate) struct NftPacket<'a> {
    pub(super) bytes: &'a [u8],
    conntrack: Option<&'a RefCell<Option<CtPacketContext>>>,
    pub(super) mark: Option<&'a Cell<u32>>,
    pub(super) redirect_address: Option<&'a RedirectAddressResolver<'a>>,
    pub(super) ipv4_addr_type: Option<&'a dyn Fn(Ipv4Address) -> u8>,
    pub(super) ipv6_local_destination: Option<&'a dyn Fn(Ipv6Address) -> bool>,
    pub(super) iifname: [u8; 16],
    pub(super) oifname: [u8; 16],
}

/// Mutable packet view for a hook that may run NAT. The VM reconstructs a
/// short-lived read-only NftPacket after each rewrite, so later chains see
/// the translated headers without holding an alias across the callback.
pub(crate) struct NftNatPacket<'a> {
    pub(super) bytes: &'a mut [u8],
    pub(super) conntrack: &'a RefCell<Option<CtPacketContext>>,
    pub(super) mark: Option<&'a Cell<u32>>,
    pub(super) redirect_address: Option<&'a RedirectAddressResolver<'a>>,
    pub(super) iifname: [u8; 16],
    pub(super) oifname: [u8; 16],
    pub(super) ipv4_addr_type: Option<&'a dyn Fn(Ipv4Address) -> u8>,
    pub(super) ipv6_local_destination: Option<&'a dyn Fn(Ipv6Address) -> bool>,
}

impl<'a> NftNatPacket<'a> {
    pub(crate) fn new_ipv4(
        bytes: &'a mut [u8],
        conntrack: &'a RefCell<Option<CtPacketContext>>,
        iifname: [u8; 16],
        oifname: [u8; 16],
        addr_type: &'a dyn Fn(Ipv4Address) -> u8,
    ) -> Self {
        Self {
            bytes,
            conntrack,
            mark: None,
            redirect_address: None,
            iifname,
            oifname,
            ipv4_addr_type: Some(addr_type),
            ipv6_local_destination: None,
        }
    }

    pub(crate) fn new_ipv6(
        bytes: &'a mut [u8],
        conntrack: &'a RefCell<Option<CtPacketContext>>,
        iifname: [u8; 16],
        oifname: [u8; 16],
    ) -> Self {
        Self {
            bytes,
            conntrack,
            mark: None,
            redirect_address: None,
            iifname,
            oifname,
            ipv4_addr_type: None,
            ipv6_local_destination: None,
        }
    }

    pub(crate) fn with_ipv6_local_destination(
        mut self,
        lookup: &'a dyn Fn(Ipv6Address) -> bool,
    ) -> Self {
        self.ipv6_local_destination = Some(lookup);
        self
    }

    pub(crate) fn with_mark(mut self, mark: &'a Cell<u32>) -> Self {
        self.mark = Some(mark);
        self
    }

    pub(crate) fn with_redirect_address(
        mut self,
        resolve: &'a dyn Fn(&[u8]) -> Option<CtAddress>,
    ) -> Self {
        self.redirect_address = Some(resolve);
        self
    }
}

impl<'a> NftPacket<'a> {
    pub(crate) fn new(bytes: &'a [u8], ipv4_addr_type: &'a dyn Fn(Ipv4Address) -> u8) -> Self {
        Self {
            bytes,
            conntrack: None,
            mark: None,
            redirect_address: None,
            ipv4_addr_type: Some(ipv4_addr_type),
            ipv6_local_destination: None,
            iifname: [0; 16],
            oifname: [0; 16],
        }
    }

    pub(crate) fn new_ipv6(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            conntrack: None,
            mark: None,
            redirect_address: None,
            ipv4_addr_type: None,
            ipv6_local_destination: None,
            iifname: [0; 16],
            oifname: [0; 16],
        }
    }

    pub(crate) fn with_ipv6_local_destination(
        mut self,
        lookup: &'a dyn Fn(Ipv6Address) -> bool,
    ) -> Self {
        self.ipv6_local_destination = Some(lookup);
        self
    }

    pub(crate) fn with_interface_names(mut self, iifname: [u8; 16], oifname: [u8; 16]) -> Self {
        self.iifname = iifname;
        self.oifname = oifname;
        self
    }

    pub(crate) fn with_conntrack_cell(
        mut self,
        context: &'a RefCell<Option<CtPacketContext>>,
    ) -> Self {
        self.conntrack = Some(context);
        self
    }

    pub(crate) fn with_mark(mut self, mark: &'a Cell<u32>) -> Self {
        self.mark = Some(mark);
        self
    }

    pub(crate) fn with_redirect_address(
        mut self,
        resolve: &'a dyn Fn(&[u8]) -> Option<CtAddress>,
    ) -> Self {
        self.redirect_address = Some(resolve);
        self
    }

    pub(super) fn ct_state_bits(&self) -> u32 {
        let context = self.conntrack.as_ref().map(|cell| cell.borrow());
        match context.as_deref().and_then(Option::as_ref) {
            Some(CtPacketContext::Untracked) => 1 << 6,
            Some(context) => match context.state() {
                Some(CtPacketState::Established) => 1 << 1,
                Some(CtPacketState::Related) => 1 << 2,
                Some(CtPacketState::New) => 1 << 3,
                Some(CtPacketState::Invalid) | None => 1,
            },
            // Before the -200 tracking hook there is no skb conntrack
            // identity. Linux's native ct state expression reads INVALID.
            None => 1,
        }
    }
}

/// Names are copied before the poller acquires FIB or smoltcp locks. A rename
/// can publish a new name later; this poll evaluates against one prior view.
pub(crate) struct NftDeviceNames(Vec<(u32, [u8; 16])>);

impl NftDeviceNames {
    pub(crate) fn empty() -> Self {
        Self(Vec::new())
    }

    pub(crate) fn snapshot(netns: &NetNamespace) -> Result<Self, SystemError> {
        let devices = netns.device_list();
        let mut names = Vec::new();
        names
            .try_reserve_exact(devices.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for (ifindex, iface) in devices.iter() {
            let ifindex = u32::try_from(*ifindex).map_err(|_| SystemError::ERANGE)?;
            let mut name = [0; 16];
            iface.common().with_iface_name(|value| {
                let bytes = value.as_bytes();
                name[..bytes.len().min(15)].copy_from_slice(&bytes[..bytes.len().min(15)]);
            });
            names.push((ifindex, name));
        }
        Ok(Self(names))
    }

    /// A missing hook device is represented by index zero. A nonzero index
    /// absent from this poll's snapshot is *not* the same thing: evaluating
    /// its name as an empty string could admit a packet through a false match.
    pub(crate) fn get(&self, ifindex: u32) -> Option<[u8; 16]> {
        if ifindex == 0 {
            return Some([0; 16]);
        }
        self.0
            .binary_search_by_key(&ifindex, |(index, _)| *index)
            .ok()
            .map(|index| self.0[index].1)
    }
}

#[cfg(test)]
mod device_name_tests {
    use super::NftDeviceNames;

    #[test]
    fn absent_device_is_distinct_from_a_hook_without_a_device() {
        let mut loopback = [0; 16];
        loopback[..2].copy_from_slice(b"lo");
        let names = NftDeviceNames(alloc::vec![(1, loopback)]);
        assert_eq!(names.get(0), Some([0; 16]));
        assert_eq!(names.get(1), Some(loopback));
        assert_eq!(names.get(2), None);
    }
}
