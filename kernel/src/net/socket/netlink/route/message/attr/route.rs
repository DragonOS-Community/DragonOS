use crate::net::socket::netlink::message::attr::Attribute;
use system_error::SystemError;

/// 路由相关属性
#[derive(Debug, Clone, Copy)]
#[repr(u16)]
enum RouteAttrClass {
    UNSPEC = 0,
    DST = 1,       // 目标地址
    SRC = 2,       // 源地址
    IIF = 3,       // 输入接口
    OIF = 4,       // 输出接口
    GATEWAY = 5,   // 网关地址
    PRIORITY = 6,  // 路由优先级
    PREFSRC = 7,   // 首选源地址
    METRICS = 8,   // 路由度量
    MULTIPATH = 9, // 多路径信息
    TABLE = 15,    // 路由表ID
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

#[derive(Debug)]
pub enum RouteAttr {
    Dst([u8; 4]),     // 目标地址 (IPv4)
    Src([u8; 4]),     // 源地址 (IPv4)
    Gateway([u8; 4]), // 网关地址 (IPv4)
    Oif(u32),         // 输出接口索引
    Iif(u32),         // 输入接口索引
    Priority(u32),    // 路由优先级
    Prefsrc([u8; 4]), // 首选源地址 (IPv4)
    Table(u32),       // 路由表ID
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
        // match self {
        //     RouteAttr::Dst(addr)
        //     | RouteAttr::Src(addr)
        //     | RouteAttr::Gateway(addr)
        //     | RouteAttr::Prefsrc(addr) => addr,
        //     RouteAttr::Oif(idx) | RouteAttr::Iif(idx) => idx.as_bytes(),
        //     RouteAttr::Priority(pri) => pri.as_bytes(),
        //     RouteAttr::Table(table) => table.as_bytes(),
        // }
        todo!()
    }

    fn read_from_buf(
        _header: &crate::net::socket::netlink::message::attr::CAttrHeader,
        _payload_buf: &[u8],
    ) -> Result<Option<Self>, SystemError>
    where
        Self: Sized,
    {
        todo!()
    }
}
