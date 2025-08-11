use crate::net::socket::netlink::message::segment::header::CMsgSegHdr;
use alloc::vec::Vec;
use system_error::SystemError;

pub mod attr;
pub mod segment;

#[derive(Debug)]
pub struct Message<T: ProtocolSegment> {
    segments: Vec<T>,
}

impl<T: ProtocolSegment> Message<T> {
    pub fn new(segments: Vec<T>) -> Self {
        Self { segments }
    }

    pub fn segments(&self) -> &[T] {
        &self.segments
    }

    pub fn segments_mut(&mut self) -> &mut [T] {
        &mut self.segments
    }

    pub fn read_from(reader: &[u8]) -> Result<Self, SystemError> {
        let segments = {
            let segment = T::read_from(reader)?;
            vec![segment]
        };

        Ok(Self { segments })
    }

    pub fn write_to(&self, writer: &mut [u8]) -> Result<usize, SystemError> {
        let total_written: usize = self
            .segments
            .iter()
            .map(|segment| segment.write_to(writer))
            .collect::<Result<Vec<usize>, SystemError>>()?
            .iter()
            .sum();

        Ok(total_written)
    }

    pub fn total_len(&self) -> usize {
        self.segments
            .iter()
            .map(|segment| segment.header().len as usize)
            .sum()
    }
}

pub trait ProtocolSegment: Sized + alloc::fmt::Debug {
    fn header(&self) -> &CMsgSegHdr;
    fn header_mut(&mut self) -> &mut CMsgSegHdr;
    fn read_from(reader: &[u8]) -> Result<Self, SystemError>;
    fn write_to(&self, writer: &mut [u8]) -> Result<usize, SystemError>;
}

pub(super) const NLMSG_ALIGN: usize = 4;
