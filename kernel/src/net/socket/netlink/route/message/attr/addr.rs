use crate::net::socket::netlink::message::attr::Attribute;
use alloc::vec::Vec;
use system_error::SystemError;

#[derive(Debug, Clone, Copy)]
#[repr(u16)]
#[expect(non_camel_case_types)]
#[expect(clippy::upper_case_acronyms)]
enum AddrAttrClass {
    UNSPEC = 0,
    ADDRESS = 1,
    LOCAL = 2,
    LABEL = 3,
    BROADCAST = 4,
    ANYCAST = 5,
    CACHEINFO = 6,
    MULTICAST = 7,
    FLAGS = 8,
    RT_PRIORITY = 9,
    TARGET_NETNSID = 10,
}

impl TryFrom<u16> for AddrAttrClass {
    type Error = SystemError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(AddrAttrClass::UNSPEC),
            1 => Ok(AddrAttrClass::ADDRESS),
            2 => Ok(AddrAttrClass::LOCAL),
            3 => Ok(AddrAttrClass::LABEL),
            4 => Ok(AddrAttrClass::BROADCAST),
            5 => Ok(AddrAttrClass::ANYCAST),
            6 => Ok(AddrAttrClass::CACHEINFO),
            7 => Ok(AddrAttrClass::MULTICAST),
            8 => Ok(AddrAttrClass::FLAGS),
            9 => Ok(AddrAttrClass::RT_PRIORITY),
            10 => Ok(AddrAttrClass::TARGET_NETNSID),
            _ => Err(SystemError::EINVAL),
        }
    }
}

#[derive(Debug, Clone)]
pub enum AddrAttr {
    Address(Vec<u8>),
    Local(Vec<u8>),
    /// Raw IFA_LABEL payload. Validation is address-family specific: IPv4
    /// uses NLA_STRING(IFNAMSIZ - 1), while Linux ignores it for IPv6.
    Label(Vec<u8>),
}

impl AddrAttr {
    fn class(&self) -> AddrAttrClass {
        match self {
            AddrAttr::Address(_) => AddrAttrClass::ADDRESS,
            AddrAttr::Local(_) => AddrAttrClass::LOCAL,
            AddrAttr::Label(_) => AddrAttrClass::LABEL,
        }
    }
}

impl Attribute for AddrAttr {
    fn type_(&self) -> u16 {
        self.class() as u16
    }

    fn payload_as_bytes(&self) -> &[u8] {
        match self {
            AddrAttr::Address(addr) => addr.as_ref(),
            AddrAttr::Local(addr) => addr.as_ref(),
            AddrAttr::Label(label) => label.as_slice(),
        }
    }

    fn read_from_buf(
        header: &crate::net::socket::netlink::message::attr::CAttrHeader,
        payload_buf: &[u8],
    ) -> Result<Option<Self>, SystemError>
    where
        Self: Sized,
    {
        let payload_len = header.payload_len();

        // TODO: Currently, `IS_NET_BYTEORDER_MASK` and `IS_NESTED_MASK` are ignored.
        let Ok(addr_class) = AddrAttrClass::try_from(header.type_()) else {
            //reader.skip_some(payload_len);
            return Ok(None);
        };

        // 拷贝payload_buf到本地变量，避免生命周期问题
        let buf = &payload_buf[..payload_len.min(payload_buf.len())];

        let res = match (addr_class, buf.len()) {
            (AddrAttrClass::ADDRESS, 4 | 16) => {
                let mut addr = vec![0u8; buf.len()];
                addr.copy_from_slice(buf);
                AddrAttr::Address(addr)
            }
            (AddrAttrClass::LOCAL, 4 | 16) => {
                let mut addr = vec![0u8; buf.len()];
                addr.copy_from_slice(buf);
                AddrAttr::Local(addr)
            }
            (AddrAttrClass::LABEL, _) => AddrAttr::Label(buf.to_vec()),
            (AddrAttrClass::ADDRESS | AddrAttrClass::LOCAL, _) => {
                log::warn!(
                    "address attribute `{:?}` contains invalid payload",
                    addr_class
                );
                return Err(SystemError::EINVAL);
            }
            (_, _) => {
                log::warn!("address attribute `{:?}` is not supported", addr_class);
                // reader.skip_some(payload_len);
                return Ok(None);
            }
        };

        Ok(Some(res))
    }
}
