use crate::net::socket::netlink::message::attr::Attribute;
use crate::net::socket::netlink::message::attr::CAttrHeader;
use crate::net::socket::netlink::route::message::attr::convert_one_from_raw_buf;
use crate::net::socket::netlink::route::message::attr::IFNAME_SIZE;
use alloc::ffi::CString;
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
}

impl TryFrom<u16> for LinkAttrClass {
    type Error = SystemError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        return <Self as FromPrimitive>::from_u16(value).ok_or(Self::Error::EINVAL);
    }
}

#[derive(Debug)]
pub enum LinkAttr {
    Name(CString),
    Mtu(u32),
    TxqLen(u32),
    LinkMode(u8),
    ExtMask(RtExtFilter),
}

impl LinkAttr {
    fn class(&self) -> LinkAttrClass {
        match self {
            LinkAttr::Name(_) => LinkAttrClass::IFNAME,
            LinkAttr::Mtu(_) => LinkAttrClass::MTU,
            LinkAttr::TxqLen(_) => LinkAttrClass::TXQLEN,
            LinkAttr::LinkMode(_) => LinkAttrClass::LINKMODE,
            LinkAttr::ExtMask(_) => LinkAttrClass::EXT_MASK,
        }
    }
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
        self.class() as u16
    }

    fn payload_as_bytes(&self) -> &[u8] {
        match self {
            LinkAttr::Name(name) => name.as_bytes_with_nul(),
            LinkAttr::Mtu(mtu) => unsafe {
                core::slice::from_raw_parts(mtu as *const u32 as *const u8, 4)
            },
            LinkAttr::TxqLen(txq_len) => unsafe {
                core::slice::from_raw_parts(txq_len as *const u32 as *const u8, 4)
            },
            LinkAttr::LinkMode(link_mode) => unsafe {
                core::slice::from_raw_parts(link_mode as *const u8, 1)
            },
            LinkAttr::ExtMask(ext_filter) => {
                let bits = ext_filter.bits();
                unsafe { core::slice::from_raw_parts(&bits as *const u32 as *const u8, 4) }
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
            // reader.skip_some(payload_len);
            return Ok(None);
        };

        let res = match (class, payload_len) {
            (LinkAttrClass::IFNAME, 1..=IFNAME_SIZE) => {
                let nul_pos = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                let cstr = CString::new(&buf[..nul_pos]).map_err(|_| SystemError::EINVAL)?;
                Self::Name(cstr)
            }
            (LinkAttrClass::MTU, 4) => {
                let data = convert_one_from_raw_buf::<u32>(buf)?;
                Self::Mtu(*data)
            }
            (LinkAttrClass::TXQLEN, 4) => {
                let data = convert_one_from_raw_buf::<u32>(buf)?;
                Self::TxqLen(*data)
            }
            (LinkAttrClass::LINKMODE, 1) => {
                let data = convert_one_from_raw_buf::<u8>(buf)?;
                Self::LinkMode(*data)
            }
            (LinkAttrClass::EXT_MASK, 4) => {
                const { assert!(size_of::<RtExtFilter>() == 4) };
                Self::ExtMask(*convert_one_from_raw_buf::<RtExtFilter>(buf)?)
            }

            (
                LinkAttrClass::IFNAME
                | LinkAttrClass::MTU
                | LinkAttrClass::TXQLEN
                | LinkAttrClass::LINKMODE
                | LinkAttrClass::EXT_MASK,
                _,
            ) => {
                log::warn!("link attribute `{:?}` contains invalid payload", class);
                return Err(SystemError::EINVAL);
            }

            (_, _) => {
                log::warn!("link attribute `{:?}` is not supported", class);
                // reader.skip_some(payload_len);
                return Ok(None);
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
