use alloc::vec::Vec;
use system_error::SystemError;

use crate::net::socket::netlink::message::{
    attr::Attribute,
    segment::{header::CMsgSegHdr, SegmentBody},
};

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
        let (body, remain_len) = Body::read_from_buf(&header, buf)?;
        let attrs = Attr::read_all_from_buf(buf, buf.len() - remain_len)?;

        Ok(Self {
            header,
            body,
            attrs,
        })
    }

    pub fn write_to_buf(&self, buf: &mut Vec<u8>) -> Result<(), SystemError> {
        if buf.len() < self.header.len as usize {
            return Err(SystemError::EINVAL);
        }

        self.body.write_to_buf(buf)?;
        for attr in self.attrs.iter() {
            attr.write_to_buf(buf)?;
        }
        Ok(())
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
