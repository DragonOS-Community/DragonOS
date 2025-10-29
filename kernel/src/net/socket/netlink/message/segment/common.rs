use crate::net::socket::netlink::message::{
    attr::Attribute,
    segment::{header::CMsgSegHdr, SegmentBody},
};
use alloc::vec::Vec;
use system_error::SystemError;

#[derive(Debug)]
pub struct SegmentCommon<Body, Attr> {
    header: CMsgSegHdr,
    body: Body,
    attrs: Vec<Attr>,
}

impl<Body, Attr> SegmentCommon<Body, Attr> {
    pub const HEADER_LEN: usize = size_of::<CMsgSegHdr>();

    pub fn header(&self) -> &CMsgSegHdr {
        &self.header
    }

    pub fn header_mut(&mut self) -> &mut CMsgSegHdr {
        &mut self.header
    }

    pub fn body(&self) -> &Body {
        &self.body
    }

    pub fn attrs(&self) -> &Vec<Attr> {
        &self.attrs
    }
}

impl<Body: SegmentBody, Attr: Attribute> SegmentCommon<Body, Attr> {
    pub const BODY_LEN: usize = size_of::<Body::CType>();

    pub fn new(header: CMsgSegHdr, body: Body, attrs: Vec<Attr>) -> Self {
        let mut res = Self {
            header,
            body,
            attrs,
        };
        res.header.len = res.total_len() as u32;
        res
    }

    pub fn read_from_buf(header: CMsgSegHdr, buf: &[u8]) -> Result<Self, SystemError> {
        // log::info!("SegmentCommon try to read from buffer");
        let (body, remain_len, padded_len) = Body::read_from_buf(&header, buf)?;

        let attrs_buf = &buf[padded_len..];
        let attrs = Attr::read_all_from_buf(attrs_buf, remain_len)?;

        Ok(Self {
            header,
            body,
            attrs,
        })
    }

    pub fn write_to_buf(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        if buf.len() < self.header.len as usize {
            return Err(SystemError::EINVAL);
        }

        // Write header to the beginning of buf
        let header_bytes = unsafe {
            core::slice::from_raw_parts(
                (&self.header as *const CMsgSegHdr) as *const u8,
                Self::HEADER_LEN,
            )
        };
        buf[..Self::HEADER_LEN].copy_from_slice(header_bytes);

        // 这里创建一个内核缓冲区，用来写入body和attribute，方便进行写入
        let mut kernel_buf: Vec<u8> = vec![];

        self.body.write_to_buf(&mut kernel_buf)?;
        for attr in self.attrs.iter() {
            attr.write_to_buf(&mut kernel_buf)?;
        }

        let actual_len = kernel_buf.len().min(buf.len());
        let payload_copied = if !kernel_buf.is_empty() {
            buf[Self::HEADER_LEN..Self::HEADER_LEN + actual_len]
                .copy_from_slice(&kernel_buf[..actual_len]);
            // log::info!("buffer: {:?}", &buf[..actual_len]);
            actual_len
        } else {
            // 如果没有数据需要写入，返回0
            // log::info!("No data to write to buffer");
            0
        };

        Ok(payload_copied + Self::HEADER_LEN)
    }

    pub fn total_len(&self) -> usize {
        Self::HEADER_LEN + Self::BODY_LEN + self.attrs_len()
    }
}

impl<Body, Attr: Attribute> SegmentCommon<Body, Attr> {
    pub fn attrs_len(&self) -> usize {
        self.attrs
            .iter()
            .map(|attr| attr.total_len_with_padding())
            .sum()
    }
}
