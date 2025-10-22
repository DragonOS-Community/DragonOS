pub(super) mod noattr;

use crate::{libs::align::align_up, net::socket::netlink::message::NLMSG_ALIGN};
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
        align_up(self.len as usize, NLMSG_ALIGN)
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

    fn write_to_buf(&self, buf: &mut Vec<u8>) -> Result<usize, SystemError> {
        let type_: u16 = self.type_();
        let payload_bytes = self.payload_as_bytes();
        let header = CAttrHeader::from_payload_len(type_, payload_bytes.len());
        let total_len = header.total_len_with_padding();

        // let mut current_offset = offset;

        // 写入头部
        let header_bytes = unsafe {
            core::slice::from_raw_parts(
                &header as *const CAttrHeader as *const u8,
                size_of::<CAttrHeader>(),
            )
        };
        // buf[current_offset..current_offset + header_bytes.len()].copy_from_slice(header_bytes);
        buf.extend_from_slice(header_bytes);
        // current_offset += header_bytes.len();

        // 写入负载
        // buf[current_offset..current_offset + payload_bytes.len()].copy_from_slice(payload_bytes);
        buf.extend_from_slice(payload_bytes);
        // current_offset += payload_bytes.len();

        // 添加对齐填充
        let padding_len = header.padding_len();
        if padding_len > 0 {
            // buf[current_offset..current_offset + padding_len].fill(0);
            buf.extend(vec![0u8; padding_len]);
        }

        Ok(total_len)
    }

    fn read_all_from_buf(buf: &[u8], mut total_len: usize) -> Result<Vec<Self>, SystemError>
    where
        Self: Sized,
    {
        let mut attrs = Vec::new();
        let mut offset = 0;

        while total_len > 0 {
            if total_len < size_of::<CAttrHeader>() {
                return Err(SystemError::EINVAL);
            }

            // 检查是否有足够的字节读取属性头部
            if buf.len() - offset < size_of::<CAttrHeader>() {
                return Err(SystemError::EINVAL);
            }

            // 读取属性头部
            let attr_header_bytes = &buf[offset..offset + size_of::<CAttrHeader>()];
            let attr_header = unsafe { *(attr_header_bytes.as_ptr() as *const CAttrHeader) };

            // 验证属性长度
            if attr_header.total_len() < size_of::<CAttrHeader>() {
                return Err(SystemError::EINVAL);
            }

            total_len = total_len
                .checked_sub(attr_header.total_len())
                .ok_or(SystemError::EINVAL)?;

            if buf.len() - offset < attr_header.total_len() {
                return Err(SystemError::EINVAL);
            }

            // 读取属性负载
            let payload_start = offset + size_of::<CAttrHeader>();
            let payload_len = attr_header.payload_len();
            let payload_buf = &buf[payload_start..payload_start + payload_len];

            // 解析属性
            if let Some(attr) = Self::read_from_buf(&attr_header, payload_buf)? {
                attrs.push(attr);
            }

            // 移动到下一个属性（考虑对齐）
            let attr_total_with_padding = attr_header.total_len_with_padding();
            offset += attr_total_with_padding;

            let padding_len = total_len.min(attr_header.padding_len());
            total_len -= padding_len;
        }

        Ok(attrs)
    }
}
