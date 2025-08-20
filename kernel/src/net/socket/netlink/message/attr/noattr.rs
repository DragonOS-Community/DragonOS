use crate::net::socket::netlink::message::attr::Attribute;
use alloc::vec::Vec;

#[derive(Debug)]
pub enum NoAttr {}

impl Attribute for NoAttr {
    fn type_(&self) -> u16 {
        match *self {}
    }

    fn payload_as_bytes(&self) -> &[u8] {
        match *self {}
    }

    fn read_from_buf(
        header: &super::CAttrHeader,
        _payload_buf: &[u8],
    ) -> Result<Option<Self>, system_error::SystemError>
    where
        Self: Sized,
    {
        let _payload_len = header.payload_len();
        //todo  reader.skip_some(payload_len);

        Ok(None)
    }

    fn read_all_from_buf(
        _buf: &[u8],
        _offset: usize,
    ) -> Result<Vec<Self>, system_error::SystemError>
    where
        Self: Sized,
    {
        Ok(Vec::new())
    }
}
