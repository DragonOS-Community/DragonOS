pub mod noattr;

use crate::net::socket::netlink::message::NLMSG_ALIGN;
use alloc::vec::Vec;
use system_error::SystemError;

const IS_NESTED_MASK: u16 = 1u16 << 15;
const IS_NET_BYTEORDER_MASK: u16 = 1u16 << 14;
const ATTRIBUTE_TYPE_MASK: u16 = !(IS_NESTED_MASK | IS_NET_BYTEORDER_MASK);

/// Netlink Attribute Header
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct CAttrHeader {
    len: u16,
    type_: u16,
}

impl CAttrHeader {
    fn from_payload_len(type_: u16, payload_len: usize) -> Self {
        let total_len = payload_len + size_of::<Self>();
        // debug_assert!(total_len <= u16::MAX as usize);

        Self {
            len: total_len as u16,
            type_,
        }
    }

    pub fn type_(&self) -> u16 {
        self.type_ & ATTRIBUTE_TYPE_MASK
    }

    pub fn payload_len(&self) -> usize {
        self.len as usize - size_of::<Self>()
    }

    pub fn total_len(&self) -> usize {
        self.len as usize
    }

    pub fn total_len_with_padding(&self) -> usize {
        (self.len as usize).checked_add(NLMSG_ALIGN - 1).unwrap() & !(NLMSG_ALIGN - 1)
    }

    pub fn padding_len(&self) -> usize {
        self.total_len_with_padding() - self.total_len()
    }
}

/// Netlink Attribute
pub trait Attribute: core::fmt::Debug + Send + Sync {
    fn type_(&self) -> u16;

    fn payload_as_bytes(&self) -> &[u8];

    fn total_len_with_padding(&self) -> usize {
        const DUMMY_TYPE: u16 = 0;

        CAttrHeader::from_payload_len(DUMMY_TYPE, self.payload_as_bytes().len())
            .total_len_with_padding()
    }

    fn read_from_buf(header: &CAttrHeader, payload_buf: &[u8]) -> Result<Option<Self>, SystemError>
    where
        Self: Sized;

    fn write_to_buf(&self, buf: &mut Vec<u8>) -> Result<(), SystemError> {
        // let payload_bytes = self.payload_as_bytes();
        // let header = CAttrHeader {
        //     len: (core::mem::size_of::<CAttrHeader>() + payload_bytes.len()) as u16,
        //     type_: self.type_(),
        // };

        // let total_len = header.len as usize;
        // let padded_len = align_to(total_len, NLMSG_ALIGN);
        // let padding_len = padded_len - total_len;

        // // 确保 buf 足够大
        // if buf.len() < offset + padded_len {
        //     buf.resize(offset + padded_len, 0);
        // }

        // // 写入头部
        // let header_bytes = unsafe {
        //     core::slice::from_raw_parts(
        //         &header as *const CAttrHeader as *const u8,
        //         core::mem::size_of::<CAttrHeader>(),
        //     )
        // };
        // buf[offset..offset + header_bytes.len()].copy_from_slice(header_bytes);

        // // 写入负载
        // buf[offset + header_bytes.len()..offset + header_bytes.len() + payload_bytes.len()]
        //     .copy_from_slice(payload_bytes);

        // // 填充部分已经在 resize 时置零，无需额外处理

        // Ok(())

        let payload_bytes = self.payload_as_bytes();
        let header = CAttrHeader {
            len: (size_of::<CAttrHeader>() + payload_bytes.len()) as u16,
            type_: self.type_(),
        };

        // 写入头部
        let header_bytes = unsafe {
            core::slice::from_raw_parts(
                &header as *const CAttrHeader as *const u8,
                size_of::<CAttrHeader>(),
            )
        };
        buf.extend_from_slice(header_bytes);

        // 写入负载
        buf.extend_from_slice(payload_bytes);

        // 添加对齐填充
        let total_len = header.len as usize;
        let padded_len = align_to(total_len, NLMSG_ALIGN);
        let padding_len = padded_len - total_len;
        if padding_len > 0 {
            buf.extend_from_slice(&vec![0u8; padding_len]);
        }

        Ok(())
    }

    fn read_all_from_buf(buf: &[u8], mut offset: usize) -> Result<Vec<Self>, SystemError>
    where
        Self: Sized,
    {
        let mut attrs = Vec::new();

        while offset < buf.len() {
            // 检查是否有足够的字节读取属性头部
            if buf.len() - offset < size_of::<CAttrHeader>() {
                return Err(SystemError::EINVAL);
            }

            // 读取属性头部
            let attr_header_bytes = &buf[offset..offset + size_of::<CAttrHeader>()];
            let attr_header = unsafe { *(attr_header_bytes.as_ptr() as *const CAttrHeader) };

            // 验证属性长度
            if attr_header.len < size_of::<CAttrHeader>() as u16 {
                return Err(SystemError::EINVAL);
            }

            let attr_total_len = attr_header.len as usize;
            if buf.len() - offset < attr_total_len {
                return Err(SystemError::EINVAL);
            }

            // 读取属性负载
            let payload_start = offset + size_of::<CAttrHeader>();
            let payload_len = attr_total_len - size_of::<CAttrHeader>();
            let payload_buf = &buf[payload_start..payload_start + payload_len];

            // 解析属性
            if let Some(attr) = Self::read_from_buf(&attr_header, payload_buf)? {
                attrs.push(attr);
            }

            // 移动到下一个属性（考虑对齐）
            let padded_len = align_to(attr_total_len, NLMSG_ALIGN);
            offset += padded_len;
        }

        Ok(attrs)
    }
}

// 辅助函数
fn align_to(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}
