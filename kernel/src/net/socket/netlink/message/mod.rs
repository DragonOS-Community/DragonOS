use crate::net::socket::netlink::{
    message::segment::header::CMsgSegHdr, table::StandardNetlinkProtocol,
};
use alloc::vec::Vec;
use system_error::SystemError;

pub(super) mod attr;
pub(super) mod segment;

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
        // log::info!("Message write_to");
        let mut total_written: usize = 0;

        for segment in self.segments() {
            if total_written >= writer.len() {
                log::warn!("Netlink write buffer is full. Some segments may be dropped.");
                break;
            }

            let remaining_buf = &mut writer[total_written..];
            let written = segment.write_to(remaining_buf)?;

            total_written += written;
        }

        // log::info!("Total written bytes: {}", total_written);
        Ok(total_written)
    }

    pub fn total_len(&self) -> usize {
        self.segments
            .iter()
            .map(|segment| segment.header().len as usize)
            .sum()
    }

    pub fn protocol(&self) -> StandardNetlinkProtocol {
        self.segments
            .first()
            .map_or(StandardNetlinkProtocol::UNUSED, |segment| {
                segment.protocol()
            })
    }
}

pub trait ProtocolSegment: Sized + alloc::fmt::Debug {
    fn header(&self) -> &CMsgSegHdr;
    fn header_mut(&mut self) -> &mut CMsgSegHdr;
    fn read_from(reader: &[u8]) -> Result<Self, SystemError>;
    fn write_to(&self, writer: &mut [u8]) -> Result<usize, SystemError>;
    fn protocol(&self) -> StandardNetlinkProtocol;
}

pub(super) const NLMSG_ALIGN: usize = 4;
