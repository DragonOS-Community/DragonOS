use crate::{
    driver::net::types::{InterfaceFlags, InterfaceType},
    net::socket::{
        netlink::{
            message::segment::{common::SegmentCommon, SegmentBody},
            route::message::attr::link::LinkAttr,
        },
        AddressFamily,
    },
};
use core::num::NonZeroU32;
use system_error::SystemError;

pub type LinkSegment = SegmentCommon<LinkSegmentBody, LinkAttr>;

impl SegmentBody for LinkSegmentBody {
    type CType = CIfinfoMsg;
}

/// `ifinfomsg`
/// <https://elixir.bootlin.com/linux/v6.13/source/include/uapi/linux/rtnetlink.h#L561>.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CIfinfoMsg {
    /// AF_UNSPEC
    pub family: u8,
    /// Padding byte
    pub pad: u8,
    /// Device type
    pub type_: u16,
    /// Interface index
    pub index: u32,
    /// Device flags
    pub flags: u32,
    /// Change mask
    pub change: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct LinkSegmentBody {
    pub family: AddressFamily,
    pub type_: InterfaceType,
    pub index: Option<NonZeroU32>,
    pub flags: InterfaceFlags,
    pub change: LinkMessageFlags,
    pub pad: Option<u8>, // Must be 0
}

impl TryFrom<CIfinfoMsg> for LinkSegmentBody {
    type Error = SystemError;

    fn try_from(value: CIfinfoMsg) -> Result<Self, Self::Error> {
        let family = AddressFamily::try_from(value.family as u16)?;
        let type_ = InterfaceType::try_from(value.type_)?;
        let index = NonZeroU32::new(value.index);
        let flags = InterfaceFlags::from_bits_truncate(value.flags);
        let change = LinkMessageFlags::from_bits_truncate(value.change);
        let pad = if value.pad > 0 { Some(value.pad) } else { None };

        Ok(Self {
            family,
            type_,
            index,
            flags,
            change,
            pad,
        })
    }
}

impl From<LinkSegmentBody> for CIfinfoMsg {
    fn from(value: LinkSegmentBody) -> Self {
        CIfinfoMsg {
            family: value.family as _,
            pad: 0u8,
            type_: value.type_ as _,
            index: value.index.map(NonZeroU32::get).unwrap_or(0),
            flags: value.flags.bits(),
            change: value.change.bits(),
        }
    }
}

bitflags! {
    /// Flags in [`CIfinfoMsg`].
    pub struct LinkMessageFlags: u32 {
        // sysfs
        const IFF_UP            = 1<<0;
        // volatile
        const IFF_BROADCAST     = 1<<1;
        // sysfs
        const IFF_DEBUG         = 1<<2;
        // volatile
        const IFF_LOOPBACK      = 1<<3;
        // volatile
        const IFF_POINTOPOINT   = 1<<4;
        // sysfs
        const IFF_NOTRAILERS    = 1<<5;
        // volatile
        const IFF_RUNNING       = 1<<6;
        // sysfs
        const IFF_NOARP         = 1<<7;
        // sysfs
        const IFF_PROMISC       = 1<<8;
        // sysfs
        const IFF_ALLMULTI      = 1<<9;
        // volatile
        const IFF_MASTER        = 1<<10;
        // volatile
        const IFF_SLAVE         = 1<<11;
        // sysfs
        const IFF_MULTICAST     = 1<<12;
        // sysfs
        const IFF_PORTSEL       = 1<<13;
        // sysfs
        const IFF_AUTOMEDIA     = 1<<14;
        // sysfs
        const IFF_DYNAMIC       = 1<<15;
    }
}
