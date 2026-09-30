//! Read/erase access to the packet-owned table. No netlink wire formats here.
//! Lock order remains table -> runtime; allocations and last Arc releases are
//! outside the table lock. Queries never observe a synthetic packet.

use super::*;
use alloc::vec::Vec;

pub(crate) const CONTROL_PAGE: usize = 32;

#[derive(Clone, Copy, Debug)]
pub(crate) struct CtRecord {
    pub(crate) serial: u64,
    pub(crate) original: CtTuple,
    pub(crate) reply: CtTuple,
    pub(crate) status: u32,
    pub(crate) timeout: u32,
    pub(crate) tcp_state: Option<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CtTupleFilter {
    pub(crate) src: Option<CtAddress>,
    pub(crate) dst: Option<CtAddress>,
    pub(crate) protocol: Option<u8>,
    pub(crate) src_port: Option<u16>,
    pub(crate) dst_port: Option<u16>,
    pub(crate) icmp_id: Option<u16>,
    pub(crate) icmp_type: Option<u8>,
    pub(crate) icmp_code: Option<u8>,
}

impl CtTupleFilter {
    fn matches(self, tuple: CtTuple) -> bool {
        if self.src.is_some_and(|src| src != tuple.src)
            || self.dst.is_some_and(|dst| dst != tuple.dst)
            || self.protocol.is_some_and(|p| p != protocol(tuple.l4))
        {
            return false;
        }
        match tuple.l4 {
            CtL4::Tcp { src_port, dst_port } | CtL4::Udp { src_port, dst_port } => {
                self.src_port.is_none_or(|p| p == src_port)
                    && self.dst_port.is_none_or(|p| p == dst_port)
            }
            CtL4::Icmp {
                identifier,
                kind,
                code,
            }
            | CtL4::Icmpv6 {
                identifier,
                kind,
                code,
            } => {
                self.icmp_id.is_none_or(|id| id == identifier)
                    && self.icmp_type.is_none_or(|t| t == kind)
                    && self.icmp_code.is_none_or(|c| c == code)
            }
            CtL4::Generic { .. } => true,
        }
    }
}

pub(crate) fn protocol(l4: CtL4) -> u8 {
    match l4 {
        CtL4::Tcp { .. } => 6,
        CtL4::Udp { .. } => 17,
        CtL4::Icmp { .. } => 1,
        CtL4::Icmpv6 { .. } => 58,
        CtL4::Generic { protocol } => protocol,
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CtFilter {
    /// AF_UNSPEC (0), AF_INET (2), AF_INET6 (10). Other families match nothing.
    pub(crate) family: u8,
    pub(crate) status: Option<(u32, u32)>,
    pub(crate) original: CtTupleFilter,
    pub(crate) reply: CtTupleFilter,
}

impl CtFilter {
    pub(crate) fn matches(self, record: &CtRecord) -> bool {
        (self.family == 0
            || matches!(
                (self.family, record.original.src),
                (2, CtAddress::V4(_)) | (10, CtAddress::V6(_))
            ))
            && self
                .status
                .is_none_or(|(value, mask)| record.status & mask == value)
            && self.original.matches(record.original)
            && self.reply.matches(record.reply)
    }
}

impl CtFlow {
    fn record(&self, now: Instant) -> Option<CtRecord> {
        let runtime = self.runtime.lock();
        if runtime.expires_at <= now {
            return None;
        }
        let tcp = runtime.tcp.as_ref();
        let seen_reply = tcp.map_or(runtime.seen_reply, TcpTracker::seen_reply);
        let assured = tcp.map_or(runtime.udp_assured, |tcp| tcp.control_state().1);
        let status = 8 // IPS_CONFIRMED
            | self.nat_done
            | (u32::from(seen_reply) << 1)
            | (u32::from(assured) << 2)
            | (u32::from(self.original.source_from(self.translated) != self.original) << 4)
            | (u32::from(self.original.destination_from(self.translated) != self.original) << 5);
        Some(CtRecord {
            serial: self.serial.load(Ordering::Relaxed),
            original: self.original,
            reply: self.reply,
            status,
            timeout: (runtime.expires_at.saturating_sub(now).total_micros() / 1_000_000)
                .min(u64::from(u32::MAX)) as u32,
            tcp_state: tcp.map(|tcp| tcp.control_state().0),
        })
    }
}

impl CtState {
    pub(crate) fn control_watermark(&self) -> u64 {
        self.table
            .lock()
            .as_ref()
            .map_or(0, |table| table.last_serial)
    }

    /// One bounded scan, not one full scan per selected record. Reserve first:
    /// Vec insertions under the spinlock cannot allocate. Arc references pin
    /// only one page and allow runtime sampling after releasing the table.
    pub(crate) fn control_page(
        &self,
        after: u64,
        through: u64,
        now: Instant,
    ) -> Result<Vec<CtRecord>, CtError> {
        let mut selected: Vec<(u64, Arc<CtFlow>)> = Vec::new();
        selected
            .try_reserve_exact(CONTROL_PAGE)
            .map_err(|_| CtError::NoMemory)?;
        let mut records = Vec::new();
        records
            .try_reserve_exact(CONTROL_PAGE)
            .map_err(|_| CtError::NoMemory)?;
        {
            let guard = self.table.lock();
            if let Some(table) = guard.as_ref() {
                for flow in table.original.values() {
                    let id = flow.serial.load(Ordering::Relaxed);
                    if id <= after || id > through {
                        continue;
                    }
                    let pos = selected.partition_point(|(old, _)| *old < id);
                    if pos < CONTROL_PAGE {
                        if selected.len() == CONTROL_PAGE {
                            // Table still owns this flow, so pop is not a last drop.
                            selected.pop();
                        }
                        selected.insert(pos, (id, flow.clone()));
                    }
                }
            }
        }
        // Expired rows still advance the cursor via serial, but lack CONFIRMED
        // status so the caller skips them. A live row can also round timeout
        // down to zero; do not confuse that with an expired placeholder.
        for (serial, flow) in selected {
            records.push(flow.record(now).unwrap_or(CtRecord {
                serial,
                original: flow.original,
                reply: flow.reply,
                status: 0,
                timeout: 0,
                tcp_state: None,
            }));
        }
        Ok(records)
    }

    pub(crate) fn control_get(&self, tuple: CtTuple, now: Instant) -> Option<CtRecord> {
        let guard = self.table.lock();
        let table = guard.as_ref()?;
        table
            .reply
            .get(&tuple)
            .or_else(|| table.original.get(&tuple))?
            .record(now)
    }

    pub(crate) fn control_delete(&self, tuple: CtTuple, id: Option<u32>, now: Instant) -> bool {
        let released = {
            let mut guard = self.table.lock();
            let Some(table) = guard.as_mut() else {
                return false;
            };
            let Some(flow) = table
                .reply
                .get(&tuple)
                .or_else(|| table.original.get(&tuple))
            else {
                return false;
            };
            if id.is_some_and(|id| id != flow.serial.load(Ordering::Relaxed) as u32)
                || flow.runtime.lock().expires_at <= now
            {
                return false;
            }
            let flow = flow.clone();
            table.remove_flow(flow)
        };
        drop(released);
        true
    }

    pub(crate) fn control_flush(&self, filter: CtFilter, now: Instant) -> usize {
        let through = self.control_watermark();
        let mut total = 0;
        loop {
            let mut released: [Option<Arc<CtFlow>>; GC_BATCH] = core::array::from_fn(|_| None);
            let count = {
                let mut guard = self.table.lock();
                let Some(table) = guard.as_mut() else {
                    return total;
                };
                let mut count = 0;
                table.original.retain(|_, flow| {
                    if count < GC_BATCH
                        && flow.serial.load(Ordering::Relaxed) <= through
                        && flow
                            .record(now)
                            .is_some_and(|record| filter.matches(&record))
                    {
                        released[count] = Some(flow.clone());
                        count += 1;
                        false
                    } else {
                        true
                    }
                });
                for flow in released.iter().take(count).flatten() {
                    table.reply.remove(&flow.reply);
                }
                if table.original.is_empty() {
                    table.next_expiry = None;
                }
                count
            };
            total += count;
            drop(released);
            if count < GC_BATCH {
                return total;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tuple() -> CtTuple {
        CtTuple {
            src: [10, 0, 0, 2].into(),
            dst: [203, 0, 113, 1].into(),
            l4: CtL4::Udp {
                src_port: 1234,
                dst_port: 53,
            },
        }
    }
    #[test]
    fn stale_id_does_not_delete_reused_tuple_or_leave_reply_index() {
        let state = CtState::new();
        state.activate(4).unwrap();
        let original = tuple();
        state
            .confirm(
                CtCandidate::new(original, CtPacketKind::Udp).unwrap(),
                Instant::ZERO,
            )
            .unwrap();
        let old = state.control_get(original, Instant::ZERO).unwrap();
        assert!(state.control_delete(
            original.reverse().unwrap(),
            Some(old.serial as u32),
            Instant::ZERO
        ));
        assert!(state.control_get(original, Instant::ZERO).is_none());
        state
            .confirm(
                CtCandidate::new(original, CtPacketKind::Udp).unwrap(),
                Instant::ZERO,
            )
            .unwrap();
        assert!(!state.control_delete(original, Some(old.serial as u32), Instant::ZERO));
        assert!(state
            .control_get(original.reverse().unwrap(), Instant::ZERO)
            .is_some());
    }
    #[test]
    fn query_is_read_only_and_udp_assurance_needs_stream_packet() {
        let state = CtState::new();
        state.activate(4).unwrap();
        let original = tuple();
        state
            .confirm(
                CtCandidate::new(original, CtPacketKind::Udp).unwrap(),
                Instant::ZERO,
            )
            .unwrap();
        state
            .lookup(
                original.reverse().unwrap(),
                CtPacketKind::Udp,
                Instant::from_secs(1),
            )
            .unwrap();
        let first = state.control_get(original, Instant::from_secs(1)).unwrap();
        assert_eq!(first.status & 6, 2);
        assert_eq!(first.timeout, 30);
        assert_eq!(
            state
                .control_get(original, Instant::from_secs(3))
                .unwrap()
                .status
                & 4,
            0
        );
        state
            .lookup(original, CtPacketKind::Udp, Instant::from_secs(3))
            .unwrap();
        let stream = state.control_get(original, Instant::from_secs(3)).unwrap();
        assert_eq!(stream.status & 6, 6);
        assert_eq!(stream.timeout, 120);
    }
}
