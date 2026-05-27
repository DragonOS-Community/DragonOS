use crate::net::socket::netlink::message::attr::{Attribute, CAttrHeader};
use crate::net::socket::netlink::route::message::attr::convert_one_from_raw_buf;
use alloc::vec::Vec;
use system_error::SystemError;

#[derive(Debug, Clone, Copy)]
#[repr(u16)]
enum RouteAttrClass {
    UNSPEC = 0,
    DST = 1,
    SRC = 2,
    IIF = 3,
    OIF = 4,
    GATEWAY = 5,
    PRIORITY = 6,
    PREFSRC = 7,
    METRICS = 8,
    MULTIPATH = 9,
    TABLE = 15,
}

impl TryFrom<u16> for RouteAttrClass {
    type Error = SystemError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(RouteAttrClass::UNSPEC),
            1 => Ok(RouteAttrClass::DST),
            2 => Ok(RouteAttrClass::SRC),
            3 => Ok(RouteAttrClass::IIF),
            4 => Ok(RouteAttrClass::OIF),
            5 => Ok(RouteAttrClass::GATEWAY),
            6 => Ok(RouteAttrClass::PRIORITY),
            7 => Ok(RouteAttrClass::PREFSRC),
            8 => Ok(RouteAttrClass::METRICS),
            9 => Ok(RouteAttrClass::MULTIPATH),
            15 => Ok(RouteAttrClass::TABLE),
            _ => Err(SystemError::EINVAL),
        }
    }
}

#[derive(Debug, Clone)]
pub enum RouteAttr {
    Dst(Vec<u8>),
    Src(Vec<u8>),
    Gateway(Vec<u8>),
    Oif(u32),
    Iif(u32),
    Priority(u32),
    Prefsrc(Vec<u8>),
    Table(u32),
}

impl RouteAttr {
    fn class(&self) -> RouteAttrClass {
        match self {
            RouteAttr::Dst(_) => RouteAttrClass::DST,
            RouteAttr::Src(_) => RouteAttrClass::SRC,
            RouteAttr::Gateway(_) => RouteAttrClass::GATEWAY,
            RouteAttr::Oif(_) => RouteAttrClass::OIF,
            RouteAttr::Iif(_) => RouteAttrClass::IIF,
            RouteAttr::Priority(_) => RouteAttrClass::PRIORITY,
            RouteAttr::Prefsrc(_) => RouteAttrClass::PREFSRC,
            RouteAttr::Table(_) => RouteAttrClass::TABLE,
        }
    }
}

impl Attribute for RouteAttr {
    fn type_(&self) -> u16 {
        self.class() as u16
    }

    fn payload_as_bytes(&self) -> &[u8] {
        match self {
            RouteAttr::Dst(addr)
            | RouteAttr::Src(addr)
            | RouteAttr::Gateway(addr)
            | RouteAttr::Prefsrc(addr) => addr.as_slice(),
            RouteAttr::Oif(idx)
            | RouteAttr::Iif(idx)
            | RouteAttr::Priority(idx)
            | RouteAttr::Table(idx) => unsafe {
                core::slice::from_raw_parts(idx as *const u32 as *const u8, 4)
            },
        }
    }

    fn read_from_buf(header: &CAttrHeader, payload_buf: &[u8]) -> Result<Option<Self>, SystemError>
    where
        Self: Sized,
    {
        let payload_len = header.payload_len();
        let Ok(class) = RouteAttrClass::try_from(header.type_()) else {
            return Ok(None);
        };

        let attr = match class {
            RouteAttrClass::DST
            | RouteAttrClass::SRC
            | RouteAttrClass::GATEWAY
            | RouteAttrClass::PREFSRC
                if matches!(payload_len, 4 | 16) =>
            {
                let bytes = payload_buf.to_vec();
                match class {
                    RouteAttrClass::DST => RouteAttr::Dst(bytes),
                    RouteAttrClass::SRC => RouteAttr::Src(bytes),
                    RouteAttrClass::GATEWAY => RouteAttr::Gateway(bytes),
                    RouteAttrClass::PREFSRC => RouteAttr::Prefsrc(bytes),
                    _ => unreachable!(),
                }
            }
            RouteAttrClass::OIF | RouteAttrClass::IIF | RouteAttrClass::PRIORITY
                if payload_len == 4 =>
            {
                let value = *convert_one_from_raw_buf::<u32>(payload_buf)?;
                match class {
                    RouteAttrClass::OIF => RouteAttr::Oif(value),
                    RouteAttrClass::IIF => RouteAttr::Iif(value),
                    RouteAttrClass::PRIORITY => RouteAttr::Priority(value),
                    _ => unreachable!(),
                }
            }
            RouteAttrClass::TABLE if payload_len == 1 => RouteAttr::Table(payload_buf[0] as u32),
            RouteAttrClass::TABLE if payload_len == 4 => {
                RouteAttr::Table(*convert_one_from_raw_buf::<u32>(payload_buf)?)
            }
            RouteAttrClass::METRICS | RouteAttrClass::MULTIPATH | RouteAttrClass::UNSPEC => {
                return Ok(None);
            }
            _ => return Err(SystemError::EINVAL),
        };

        Ok(Some(attr))
    }
}
