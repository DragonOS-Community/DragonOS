use crate::net::socket::netlink::{
    message::{
        segment::{
            ack::DoneSegment,
            header::{CMsgSegHdr, SegHdrCommonFlags},
        },
        ProtocolSegment,
    },
    route::message::segment::RouteNlSegment,
};
use alloc::vec::Vec;

pub fn finish_response(
    request_header: &CMsgSegHdr,
    dump_all: bool,
    response_segments: &mut Vec<RouteNlSegment>,
) {
    if !dump_all {
        assert_eq!(response_segments.len(), 1);
        return;
    }

    append_done_segment(request_header, response_segments);
    add_multi_flag(response_segments);
}

fn append_done_segment(header: &CMsgSegHdr, response_segments: &mut Vec<RouteNlSegment>) {
    let done_segment = DoneSegment::new_from_request(header, None);
    response_segments.push(RouteNlSegment::Done(done_segment));
}

fn add_multi_flag(responce_segment: &mut [RouteNlSegment]) {
    for segment in responce_segment.iter_mut() {
        let header = segment.header_mut();
        let mut flags = SegHdrCommonFlags::from_bits_truncate(header.flags);
        flags |= SegHdrCommonFlags::MULTI;
        header.flags = flags.bits();
    }
}
