use system_error::SystemError;

use crate::net::socket::{
    netlink::{
        message::segment::{common::SegmentCommon, SegmentBody},
        route::message::attr::route::RouteAttr,
    },
    AddressFamily,
};

pub type RouteSegment = SegmentCommon<RouteSegmentBody, RouteAttr>;

impl SegmentBody for RouteSegmentBody {
    type CType = CRtMsg;
}

/// `rtmsg` in Linux
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CRtMsg {
    /// 地址族 (AF_INET/AF_INET6)
    pub family: u8,
    /// 目标地址前缀长度
    pub dst_len: u8,
    /// 源地址前缀长度
    pub src_len: u8,
    /// 服务类型/DSCP
    pub tos: u8,
    /// 路由表ID
    pub table: u8,
    /// 路由协议
    pub protocol: u8,
    /// 路由作用域
    pub scope: u8,
    /// 路由类型
    pub type_: u8,
    /// 路由标志
    pub flags: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct RouteSegmentBody {
    pub family: AddressFamily,
    pub dst_len: u8,
    pub src_len: u8,
    pub tos: u8,
    pub table: RouteTable,
    pub protocol: RouteProtocol,
    pub scope: RouteScope,
    pub type_: RouteType,
    pub flags: RouteFlags,
}

// 定义路由相关的枚举类型
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum RouteTable {
    Unspec = 0,
    Compat = 252,
    Default = 253,
    Main = 254,
    Local = 255,
}

impl TryFrom<u8> for RouteTable {
    type Error = SystemError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(RouteTable::Unspec),
            252 => Ok(RouteTable::Compat),
            253 => Ok(RouteTable::Default),
            254 => Ok(RouteTable::Main),
            255 => Ok(RouteTable::Local),
            _ => Err(SystemError::EINVAL),
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum RouteProtocol {
    Unspec = 0,
    Redirect = 1,
    Kernel = 2,
    Boot = 3,
    Static = 4,
    // 添加更多协议...
}

impl TryFrom<u8> for RouteProtocol {
    type Error = SystemError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(RouteProtocol::Unspec),
            1 => Ok(RouteProtocol::Redirect),
            2 => Ok(RouteProtocol::Kernel),
            3 => Ok(RouteProtocol::Boot),
            4 => Ok(RouteProtocol::Static),
            _ => Err(SystemError::EINVAL),
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum RouteScope {
    Universe = 0,
    Site = 200,
    Link = 253,
    Host = 254,
    Nowhere = 255,
}

impl TryFrom<u8> for RouteScope {
    type Error = SystemError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(RouteScope::Universe),
            200 => Ok(RouteScope::Site),
            253 => Ok(RouteScope::Link),
            254 => Ok(RouteScope::Host),
            255 => Ok(RouteScope::Nowhere),
            _ => Err(SystemError::EINVAL),
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum RouteType {
    Unspec = 0,
    Unicast = 1,
    Local = 2,
    Broadcast = 3,
    Anycast = 4,
    Multicast = 5,
    Blackhole = 6,
    Unreachable = 7,
    Prohibit = 8,
}

impl TryFrom<u8> for RouteType {
    type Error = SystemError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(RouteType::Unspec),
            1 => Ok(RouteType::Unicast),
            2 => Ok(RouteType::Local),
            3 => Ok(RouteType::Broadcast),
            4 => Ok(RouteType::Anycast),
            5 => Ok(RouteType::Multicast),
            6 => Ok(RouteType::Blackhole),
            7 => Ok(RouteType::Unreachable),
            8 => Ok(RouteType::Prohibit),
            _ => Err(SystemError::EINVAL),
        }
    }
}

bitflags::bitflags! {
    pub struct RouteFlags: u32 {
        const NOTIFY = 0x100;
        const CLONED = 0x200;
        const EQUALIZE = 0x400;
        const PREFIX = 0x800;
    }
}

impl TryFrom<CRtMsg> for RouteSegmentBody {
    type Error = SystemError;

    fn try_from(value: CRtMsg) -> Result<Self, Self::Error> {
        let family = AddressFamily::try_from(value.family as u16)?;
        let table = RouteTable::try_from(value.table)?;
        let protocol = RouteProtocol::try_from(value.protocol)?;
        let scope = RouteScope::try_from(value.scope)?;
        let type_ = RouteType::try_from(value.type_)?;
        let flags = RouteFlags::from_bits_truncate(value.flags);

        Ok(Self {
            family,
            dst_len: value.dst_len,
            src_len: value.src_len,
            tos: value.tos,
            table,
            protocol,
            scope,
            type_,
            flags,
        })
    }
}
