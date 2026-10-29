use crate::net::socket::netlink::message::attr::Attribute;
use crate::net::socket::netlink::message::attr::CAttrHeader;
use crate::net::socket::netlink::route::message::attr::IFNAME_SIZE;
use alloc::ffi::CString;
use alloc::vec::Vec;
use num_traits::FromPrimitive;
use system_error::SystemError;

#[derive(Debug, Clone, Copy, FromPrimitive, ToPrimitive)]
#[repr(u16)]
#[expect(non_camel_case_types)]
#[expect(clippy::upper_case_acronyms)]
enum LinkAttrClass {
    UNSPEC = 0,
    ADDRESS = 1,
    BROADCAST = 2,
    IFNAME = 3,
    MTU = 4,
    LINK = 5,
    QDISC = 6,
    STATS = 7,
    COST = 8,
    PRIORITY = 9,
    MASTER = 10,
    /// Wireless Extension event
    WIRELESS = 11,
    /// Protocol specific information for a link
    PROTINFO = 12,
    TXQLEN = 13,
    MAP = 14,
    WEIGHT = 15,
    OPERSTATE = 16,
    LINKMODE = 17,
    LINKINFO = 18,
    NET_NS_PID = 19,
    IFALIAS = 20,
    /// Number of VFs if device is SR-IOV PF
    NUM_VF = 21,
    VFINFO_LIST = 22,
    STATS64 = 23,
    VF_PORTS = 24,
    PORT_SELF = 25,
    AF_SPEC = 26,
    /// Group the device belongs to
    GROUP = 27,
    NET_NS_FD = 28,
    /// Extended info mask, VFs, etc.
    EXT_MASK = 29,
    /// Promiscuity count: > 0 means acts PROMISC
    PROMISCUITY = 30,
    NUM_TX_QUEUES = 31,
    NUM_RX_QUEUES = 32,
    CARRIER = 33,
    PHYS_PORT_ID = 34,
    CARRIER_CHANGES = 35,
    PHYS_SWITCH_ID = 36,
    LINK_NETNSID = 37,
    PHYS_PORT_NAME = 38,
    PROTO_DOWN = 39,
    GSO_MAX_SEGS = 40,
    GSO_MAX_SIZE = 41,
    PAD = 42,
    XDP = 43,
    EVENT = 44,
    NEW_NETNSID = 45,
    IF_NETNSID = 46,
    CARRIER_UP_COUNT = 47,
    CARRIER_DOWN_COUNT = 48,
    NEW_IFINDEX = 49,
    MIN_MTU = 50,
    MAX_MTU = 51,
    PROP_LIST = 52,
    /// Alternative ifname
    ALT_IFNAME = 53,
    PERM_ADDRESS = 54,
    PROTO_DOWN_REASON = 55,
    PARENT_DEV_NAME = 56,
    PARENT_DEV_BUS_NAME = 57,
    GRO_MAX_SIZE = 58,
    TSO_MAX_SIZE = 59,
    TSO_MAX_SEGS = 60,
    /// All-multicast count: > 0 means acts ALLMULTI.
    ALLMULTI = 61,
}

impl TryFrom<u16> for LinkAttrClass {
    type Error = SystemError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        return <Self as FromPrimitive>::from_u16(value).ok_or(Self::Error::EINVAL);
    }
}

#[derive(Debug, Clone)]
pub enum LinkAttr {
    Address(Vec<u8>),
    Name(CString),
    Mtu(u32),
    Link(u32),
    Master(u32),
    LinkInfo(Vec<u8>),
    NetNsPid(u32),
    NetNsFd(u32),
    LinkNetnsid(u32),
    NewIfIndex(u32),
    Promiscuity(u32),
    Allmulti(u32),
    TxqLen(u32),
    LinkMode(u8),
    ExtMask(RtExtFilter),
    /// Preserve unrecognized input so mutating requests cannot silently succeed.
    Unsupported(u16, Vec<u8>),
}

impl LinkAttr {
    fn class(&self) -> LinkAttrClass {
        match self {
            LinkAttr::Address(_) => LinkAttrClass::ADDRESS,
            LinkAttr::Name(_) => LinkAttrClass::IFNAME,
            LinkAttr::Mtu(_) => LinkAttrClass::MTU,
            LinkAttr::Link(_) => LinkAttrClass::LINK,
            LinkAttr::Master(_) => LinkAttrClass::MASTER,
            LinkAttr::LinkInfo(_) => LinkAttrClass::LINKINFO,
            LinkAttr::NetNsPid(_) => LinkAttrClass::NET_NS_PID,
            LinkAttr::NetNsFd(_) => LinkAttrClass::NET_NS_FD,
            LinkAttr::LinkNetnsid(_) => LinkAttrClass::LINK_NETNSID,
            LinkAttr::NewIfIndex(_) => LinkAttrClass::NEW_IFINDEX,
            LinkAttr::Promiscuity(_) => LinkAttrClass::PROMISCUITY,
            LinkAttr::Allmulti(_) => LinkAttrClass::ALLMULTI,
            LinkAttr::TxqLen(_) => LinkAttrClass::TXQLEN,
            LinkAttr::LinkMode(_) => LinkAttrClass::LINKMODE,
            LinkAttr::ExtMask(_) => LinkAttrClass::EXT_MASK,
            LinkAttr::Unsupported(_, _) => LinkAttrClass::UNSPEC,
        }
    }
}

/// One validated child NLA. Nested link attributes have a bounded, fixed
/// depth (LINKINFO -> INFO_DATA -> VETH_INFO_PEER), so no recursive parser is
/// needed.
pub(crate) struct NestedLinkAttr<'a> {
    pub kind: u16,
    pub payload: &'a [u8],
}

pub(crate) fn parse_nested_link_attrs(data: &[u8]) -> Result<Vec<NestedLinkAttr<'_>>, SystemError> {
    let mut attrs = Vec::new();
    let mut offset = 0usize;
    while offset < data.len() {
        let rest = &data[offset..];
        if rest.len() < 4 {
            return Err(SystemError::EINVAL);
        }
        let len = u16::from_ne_bytes([rest[0], rest[1]]) as usize;
        let kind = u16::from_ne_bytes([rest[2], rest[3]]) & 0x3fff;
        if len < 4 || len > rest.len() {
            return Err(SystemError::EINVAL);
        }
        attrs.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        attrs.push(NestedLinkAttr {
            kind,
            payload: &rest[4..len],
        });
        let aligned = len.checked_add(3).ok_or(SystemError::EINVAL)? & !3;
        offset += if aligned <= rest.len() { aligned } else { len };
    }
    Ok(attrs)
}

pub(crate) fn append_nested_link_attr(
    output: &mut Vec<u8>,
    kind: u16,
    payload: &[u8],
) -> Result<(), SystemError> {
    let len = payload.len().checked_add(4).ok_or(SystemError::EINVAL)?;
    let len = u16::try_from(len).map_err(|_| SystemError::EINVAL)?;
    let aligned = (len as usize + 3) & !3;
    output
        .try_reserve(aligned)
        .map_err(|_| SystemError::ENOMEM)?;
    output.extend_from_slice(&len.to_ne_bytes());
    output.extend_from_slice(&kind.to_ne_bytes());
    output.extend_from_slice(payload);
    output.resize(output.len() + aligned - len as usize, 0);
    Ok(())
}

fn copy_payload(buf: &[u8]) -> Result<Vec<u8>, SystemError> {
    let mut data = Vec::new();
    data.try_reserve_exact(buf.len())
        .map_err(|_| SystemError::ENOMEM)?;
    data.extend_from_slice(buf);
    Ok(data)
}

// #[derive(Debug)]
// pub enum LinkInfoAttr{
//     Kind(CString),
//     Data(Vec<LinkInfoDataAttr>),
// }

// #[derive(Debug)]
// pub enum LinkInfoDataAttr{
//     VlanId(u16),

// }

impl Attribute for LinkAttr {
    fn type_(&self) -> u16 {
        match self {
            Self::Unsupported(kind, _) => *kind,
            _ => self.class() as u16,
        }
    }

    fn payload_as_bytes(&self) -> &[u8] {
        match self {
            LinkAttr::Address(address) => address.as_slice(),
            LinkAttr::Name(name) => name.as_bytes_with_nul(),
            LinkAttr::Mtu(mtu) => unsafe {
                core::slice::from_raw_parts(mtu as *const u32 as *const u8, 4)
            },
            LinkAttr::Link(value)
            | LinkAttr::Master(value)
            | LinkAttr::NetNsPid(value)
            | LinkAttr::NetNsFd(value)
            | LinkAttr::LinkNetnsid(value)
            | LinkAttr::NewIfIndex(value) => unsafe {
                core::slice::from_raw_parts(value as *const u32 as *const u8, 4)
            },
            LinkAttr::LinkInfo(data) | LinkAttr::Unsupported(_, data) => data.as_slice(),
            LinkAttr::Promiscuity(count) | LinkAttr::Allmulti(count) => unsafe {
                core::slice::from_raw_parts(count as *const u32 as *const u8, 4)
            },
            LinkAttr::TxqLen(txq_len) => unsafe {
                core::slice::from_raw_parts(txq_len as *const u32 as *const u8, 4)
            },
            LinkAttr::LinkMode(link_mode) => unsafe {
                core::slice::from_raw_parts(link_mode as *const u8, 1)
            },
            LinkAttr::ExtMask(ext_filter) => {
                const { assert!(size_of::<RtExtFilter>() == 4) };
                unsafe {
                    core::slice::from_raw_parts(ext_filter as *const RtExtFilter as *const u8, 4)
                }
            }
        }
    }

    fn read_from_buf(header: &CAttrHeader, buf: &[u8]) -> Result<Option<Self>, SystemError>
    where
        Self: Sized,
    {
        let payload_len = header.payload_len();

        // TODO: Currently, `IS_NET_BYTEORDER_MASK` and `IS_NESTED_MASK` are ignored.
        let Ok(class) = LinkAttrClass::try_from(header.type_()) else {
            return Ok(Some(Self::Unsupported(header.type_(), copy_payload(buf)?)));
        };

        let res = match (class, payload_len) {
            (LinkAttrClass::IFNAME, 1..=IFNAME_SIZE) => {
                let nul_pos = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                let cstr = CString::new(&buf[..nul_pos]).map_err(|_| SystemError::EINVAL)?;
                Self::Name(cstr)
            }
            (LinkAttrClass::MTU, 4) => Self::Mtu(u32::from_ne_bytes(buf.try_into().unwrap())),
            (LinkAttrClass::LINK, 4) => Self::Link(u32::from_ne_bytes(buf.try_into().unwrap())),
            (LinkAttrClass::MASTER, 4) => Self::Master(u32::from_ne_bytes(buf.try_into().unwrap())),
            (LinkAttrClass::LINKINFO, _) => {
                parse_nested_link_attrs(buf)?;
                Self::LinkInfo(copy_payload(buf)?)
            }
            (LinkAttrClass::NET_NS_FD, 4) => {
                Self::NetNsFd(u32::from_ne_bytes(buf.try_into().unwrap()))
            }
            (LinkAttrClass::NET_NS_PID, 4) => {
                Self::NetNsPid(u32::from_ne_bytes(buf.try_into().unwrap()))
            }
            (LinkAttrClass::LINK_NETNSID, 4) => {
                Self::LinkNetnsid(u32::from_ne_bytes(buf.try_into().unwrap()))
            }
            (LinkAttrClass::NEW_IFINDEX, 4) => {
                Self::NewIfIndex(u32::from_ne_bytes(buf.try_into().unwrap()))
            }
            (LinkAttrClass::ADDRESS, 1..=32) => Self::Address(copy_payload(buf)?),
            (LinkAttrClass::PROMISCUITY, 4) => {
                Self::Promiscuity(u32::from_ne_bytes(buf.try_into().unwrap()))
            }
            (LinkAttrClass::ALLMULTI, 4) => {
                Self::Allmulti(u32::from_ne_bytes(buf.try_into().unwrap()))
            }
            (LinkAttrClass::TXQLEN, 4) => Self::TxqLen(u32::from_ne_bytes(buf.try_into().unwrap())),
            (LinkAttrClass::LINKMODE, 1) => Self::LinkMode(buf[0]),
            (LinkAttrClass::EXT_MASK, 4) => {
                const { assert!(size_of::<RtExtFilter>() == 4) };
                Self::ExtMask(RtExtFilter::from_bits_truncate(u32::from_ne_bytes(
                    buf.try_into().unwrap(),
                )))
            }

            (
                LinkAttrClass::IFNAME
                | LinkAttrClass::MTU
                | LinkAttrClass::LINK
                | LinkAttrClass::MASTER
                | LinkAttrClass::NET_NS_FD
                | LinkAttrClass::NET_NS_PID
                | LinkAttrClass::LINK_NETNSID
                | LinkAttrClass::NEW_IFINDEX
                | LinkAttrClass::ADDRESS
                | LinkAttrClass::PROMISCUITY
                | LinkAttrClass::ALLMULTI
                | LinkAttrClass::TXQLEN
                | LinkAttrClass::LINKMODE
                | LinkAttrClass::EXT_MASK,
                _,
            ) => {
                log::warn!("link attribute `{:?}` contains invalid payload", class);
                return Err(SystemError::EINVAL);
            }

            (_, _) => {
                return Ok(Some(Self::Unsupported(class as u16, copy_payload(buf)?)));
            }
        };

        Ok(Some(res))
    }
}

bitflags! {
    /// New extended info filters for [`NlLinkAttr::ExtMask`].
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.13/source/include/uapi/linux/rtnetlink.h#L819>.
    #[repr(C)]
    pub struct RtExtFilter: u32 {
        const VF = 1 << 0;
        const BRVLAN = 1 << 1;
        const BRVLAN_COMPRESSED = 1 << 2;
        const SKIP_STATS = 1 << 3;
        const MRP = 1 << 4;
        const CFM_CONFIG = 1 << 5;
        const CFM_STATUS = 1 << 6;
        const MST = 1 << 7;
    }
}
