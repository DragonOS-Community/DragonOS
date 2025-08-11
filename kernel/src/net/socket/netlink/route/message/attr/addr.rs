use crate::net::socket::netlink::{message::attr::Attribute, route::message::attr::IFNAME_SIZE};
use alloc::ffi::CString;
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

#[derive(Debug)]
pub enum AddrAttr {
    Address([u8; 4]),
    Local([u8; 4]),
    Label(CString),
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
            AddrAttr::Label(label) => label.to_bytes_with_nul(),
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

        let Ok(addr_class) = AddrAttrClass::try_from(header.type_()) else {
            //todo 或许这里我应该返回偏移值
            //reader.skip_some(payload_len);
            return Ok(None);
        };

        // 拷贝payload_buf到本地变量，避免生命周期问题
        let buf = &payload_buf[..payload_len.min(payload_buf.len())];

        let res = match (addr_class, buf.len()) {
            (AddrAttrClass::ADDRESS, 4) => {
                let mut arr = [0u8; 4];
                arr.copy_from_slice(&buf[0..4]);
                AddrAttr::Address(arr)
            }
            (AddrAttrClass::LOCAL, 4) => {
                let mut arr = [0u8; 4];
                arr.copy_from_slice(&buf[0..4]);
                AddrAttr::Local(arr)
            }
            (AddrAttrClass::LABEL, 1..=IFNAME_SIZE) => {
                // 查找第一个0字节作为结尾，否则用全部
                let nul_pos = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                let cstr = CString::new(&buf[..nul_pos]).map_err(|_| SystemError::EINVAL)?;
                AddrAttr::Label(cstr)
            }
            (AddrAttrClass::ADDRESS | AddrAttrClass::LOCAL | AddrAttrClass::LABEL, _) => {
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
