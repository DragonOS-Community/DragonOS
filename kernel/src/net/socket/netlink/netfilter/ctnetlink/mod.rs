//! Query/cleanup control plane for the namespace's real packet table.
mod encode;
mod parse;
use super::{
    nft::{done_message, GetTableResult, NftSocketState},
    NetfilterMessage, Request, HEADER_LEN, NFGEN_LEN,
};
use crate::{
    net::{
        conntrack::{CtError, CtFilter},
        socket::netlink::table::{NetlinkNetfilterProtocol, SupportedNetlinkProtocol},
    },
    process::namespace::net_namespace::NetNamespace,
    time::Instant,
};
use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;
const CT_GET: u16 = 0x101;
const CT_DELETE: u16 = 0x102;
#[derive(Debug)]
pub(super) struct CtDump {
    filter: CtFilter,
    flags: u16,
    sequence: u32,
    cursor: u64,
    through: u64,
}
fn ct_error(error: CtError) -> SystemError {
    match error {
        CtError::NoMemory => SystemError::ENOMEM,
        _ => SystemError::EINVAL,
    }
}
pub(super) fn request(
    request: &Request<'_>,
    port: u32,
    netns: &Arc<NetNamespace>,
    state: &NftSocketState,
) -> Result<GetTableResult, SystemError> {
    let family = request.bytes[HEADER_LEN];
    let attrs = parse::CtAttrs::new(&request.bytes[HEADER_LEN + NFGEN_LEN..])?;
    let now = Instant::now();
    if request.kind() == CT_GET && request.flags() & 0x300 != 0 {
        let filter = attrs.filter(family, false)?;
        state.start_conntrack_dump(
            CtDump {
                filter,
                flags: 2 | if attrs.needs_filter(family) { 0x20 } else { 0 },
                sequence: u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap()),
                cursor: 0,
                through: netns.conntrack().control_watermark(),
            },
            port,
            netns,
        )?;
        return Ok(GetTableResult::DumpStarted);
    }
    if request.kind() == CT_GET || request.kind() == CT_DELETE {
        if let Some(tuple) = attrs.exact(family)? {
            if request.kind() == CT_GET {
                let record = netns
                    .conntrack()
                    .control_get(tuple, now)
                    .ok_or(SystemError::ENOENT)?;
                let message = encode::record(
                    &record,
                    u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap()),
                    port,
                    0,
                )?;
                NetlinkNetfilterProtocol::unicast(port, message, netns.clone())?;
            } else if !netns.conntrack().control_delete(tuple, attrs.id(), now) {
                return Err(SystemError::ENOENT);
            }
        } else if request.kind() == CT_DELETE {
            let family = if request.bytes[HEADER_LEN + 1] == 0 {
                0
            } else {
                family
            };
            let filter = attrs.filter(family, true)?;
            netns.conntrack().control_flush(filter, now);
        } else {
            return Err(SystemError::EINVAL);
        }
        Ok(GetTableResult::Replied)
    } else {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }
}
impl CtDump {
    /// One page is one queued datagram; no full-table snapshot is pinned to a
    /// socket which stops receiving. Advance only after successful unicast.
    pub(super) fn drive(
        &mut self,
        port: u32,
        netns: &Arc<NetNamespace>,
    ) -> Result<bool, SystemError> {
        let mut bytes = Vec::new();
        bytes.try_reserve(8192).map_err(|_| SystemError::ENOMEM)?;
        let mut cursor = self.cursor;
        loop {
            let page = netns
                .conntrack()
                .control_page(cursor, self.through, Instant::now())
                .map_err(ct_error)?;
            if page.is_empty() {
                let done = done_message(self.sequence, port, false)?;
                bytes
                    .try_reserve(done.0.len())
                    .map_err(|_| SystemError::ENOMEM)?;
                bytes.extend_from_slice(&done.0);
                NetlinkNetfilterProtocol::unicast(
                    port,
                    NetfilterMessage::new(bytes)?,
                    netns.clone(),
                )?;
                self.cursor = cursor;
                return Ok(true);
            }
            for record in page {
                cursor = record.serial;
                if record.status & 8 != 0 && self.filter.matches(&record) {
                    let message = encode::record(&record, self.sequence, port, self.flags)?;
                    bytes
                        .try_reserve(message.0.len())
                        .map_err(|_| SystemError::ENOMEM)?;
                    bytes.extend_from_slice(&message.0);
                }
            }
            if !bytes.is_empty() {
                NetlinkNetfilterProtocol::unicast(
                    port,
                    NetfilterMessage::new(bytes)?,
                    netns.clone(),
                )?;
                self.cursor = cursor;
                return Ok(false);
            }
        }
    }
}
