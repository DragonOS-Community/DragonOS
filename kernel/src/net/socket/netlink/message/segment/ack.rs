use crate::net::socket::netlink::message::{
    attr::noattr::NoAttr,
    segment::{
        common::SegmentCommon,
        header::{CMsgSegHdr, SegHdrCommonFlags},
        CSegmentType, SegmentBody,
    },
};
use alloc::vec::Vec;
use system_error::SystemError;

pub type DoneSegment = SegmentCommon<DoneSegmentBody, NoAttr>;

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct DoneSegmentBody {
    error_code: i32,
}

impl SegmentBody for DoneSegmentBody {
    type CType = DoneSegmentBody;
}

impl DoneSegment {
    pub fn new_from_request(request_header: &CMsgSegHdr, error: Option<SystemError>) -> Self {
        let header = CMsgSegHdr {
            len: 0,
            type_: CSegmentType::DONE as _,
            flags: SegHdrCommonFlags::empty().bits(),
            seq: request_header.seq,
            pid: request_header.pid,
        };

        let body = {
            let error_code = if let Some(err) = error {
                err.to_posix_errno()
            } else {
                0
            };
            DoneSegmentBody { error_code }
        };

        Self::new(header, body, Vec::new())
    }
}

pub type ErrorSegment = SegmentCommon<ErrorSegmentBody, NoAttr>;

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct ErrorSegmentBody {
    error_code: i32,
    request_header: CMsgSegHdr,
}

impl SegmentBody for ErrorSegmentBody {
    type CType = ErrorSegmentBody;
}

impl ErrorSegment {
    pub fn new_from_request(request_header: &CMsgSegHdr, error: Option<SystemError>) -> Self {
        let header = CMsgSegHdr {
            len: 0,
            type_: CSegmentType::ERROR as _,
            flags: SegHdrCommonFlags::empty().bits(),
            seq: request_header.seq,
            pid: request_header.pid,
        };

        let body = {
            let error_code = if let Some(err) = error {
                err.to_posix_errno()
            } else {
                0
            };
            ErrorSegmentBody {
                error_code,
                request_header: *request_header,
            }
        };

        Self::new(header, body, Vec::new())
    }
}
