use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use system_error::SystemError;

use crate::bpf::classic::{self, validate_cbpf, SockFilter};
use crate::net::socket::common::{
    parse_socket_buffer_size, parse_timeval_ticks, write_i32_getsockopt, write_timeval_ticks,
    write_u32_getsockopt, INFINITE_TIMEOUT_TICKS, SOCK_MIN_RCVBUF, SOCK_MIN_SNDBUF,
    SYSCTL_RMEM_MAX, SYSCTL_WMEM_MAX,
};
use crate::net::socket::{PSO, PSOL};
use crate::rcu::rcu_defer_drop;

use super::ring::PacketRing;
use super::uapi::tpacket_version;
use super::{packet_option, PacketSocket};
impl PacketSocket {
    fn socket_timeout_ticks(&self, name: usize) -> Result<&AtomicU64, SystemError> {
        match PSO::try_from(name as u32) {
            Ok(PSO::SNDTIMEO_OLD) | Ok(PSO::SNDTIMEO_NEW) => Ok(&self.send_timeout_ticks),
            Ok(PSO::RCVTIMEO_OLD) | Ok(PSO::RCVTIMEO_NEW) => Ok(&self.recv_timeout_ticks),
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn parse_i32(value: &[u8]) -> Result<i32, SystemError> {
        if value.len() < 4 {
            return Err(SystemError::EINVAL);
        }
        Ok(i32::from_ne_bytes(value[..4].try_into().unwrap()))
    }

    fn parse_u32_exact(value: &[u8]) -> Result<u32, SystemError> {
        if value.len() != core::mem::size_of::<u32>() {
            return Err(SystemError::EINVAL);
        }
        Ok(u32::from_ne_bytes(value.try_into().unwrap()))
    }

    fn parse_fanout(value: &[u8]) -> Result<(u32, u32), SystemError> {
        if value.len() != core::mem::size_of::<u32>()
            && value.len() != 2 * core::mem::size_of::<u32>()
        {
            return Err(SystemError::EINVAL);
        }
        // Linux lays out the first four bytes so that native u32 decoding is
        // always `id | type_flags << 16`, including big-endian targets.
        let raw = u32::from_ne_bytes(value[..4].try_into().unwrap());
        let max_num_members = if value.len() == 8 {
            u32::from_ne_bytes(value[4..8].try_into().unwrap())
        } else {
            0
        };
        Ok((raw, max_num_members))
    }
    pub(super) fn packet_option(
        &self,
        level: PSOL,
        name: usize,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        match level {
            PSOL::SOCKET => self.get_socket_option(name, value),
            PSOL::PACKET => match name {
                packet_option::PACKET_STATISTICS => {
                    if value.len() < 8 {
                        return Err(SystemError::EINVAL);
                    }
                    // Linux clear-on-read semantics: tp_packets = accepted + drops.
                    let drops = self.stats_drops.swap(0, Ordering::Relaxed);
                    let packets = self.stats_packets.swap(0, Ordering::Relaxed);
                    let total = packets.wrapping_add(drops);
                    value[..4].copy_from_slice(&total.to_ne_bytes());
                    value[4..8].copy_from_slice(&drops.to_ne_bytes());
                    Ok(8)
                }
                packet_option::PACKET_AUXDATA => Ok(write_i32_getsockopt(
                    value,
                    self.options.read().auxdata as i32,
                )),
                packet_option::PACKET_VERSION => {
                    let v = match self.ring_state.lock().version {
                        super::ring::TpacketVersion::V1 => tpacket_version::TPACKET_V1,
                        super::ring::TpacketVersion::V2 => tpacket_version::TPACKET_V2,
                    };
                    Ok(write_i32_getsockopt(value, v))
                }
                packet_option::PACKET_RESERVE => {
                    Ok(write_u32_getsockopt(value, self.ring_state.lock().reserve))
                }
                packet_option::PACKET_HDRLEN => {
                    // Linux in/out semantics: the caller passes the TPACKET
                    // version in optval[0..4]; we return sizeof(hdr) for that
                    // version (V1=32 on the supported 64-bit ABI, V2=32).
                    let v = Self::parse_i32(value)?;
                    let hdrlen = match v {
                        tpacket_version::TPACKET_V1 => {
                            core::mem::size_of::<super::uapi::TpacketHdr>() as i32
                        }
                        tpacket_version::TPACKET_V2 => {
                            core::mem::size_of::<super::uapi::Tpacket2Hdr>() as i32
                        }
                        _ => return Err(SystemError::EINVAL),
                    };
                    Ok(write_i32_getsockopt(value, hdrlen))
                }
                packet_option::PACKET_FANOUT => {
                    // Linux packet_getsockopt: an unjoined socket reports 0.
                    let val = self.fanout_getsockopt_value().unwrap_or(0) as i32;
                    Ok(write_i32_getsockopt(value, val))
                }
                _ => Err(SystemError::ENOPROTOOPT),
            },
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }
    pub(super) fn set_packet_option(
        &self,
        level: PSOL,
        name: usize,
        value: &[u8],
    ) -> Result<(), SystemError> {
        match level {
            PSOL::SOCKET => self.set_socket_option(name, value),
            PSOL::PACKET => match name {
                packet_option::PACKET_ADD_MEMBERSHIP => self.add_membership(value),
                packet_option::PACKET_DROP_MEMBERSHIP => self.drop_membership(value),
                packet_option::PACKET_AUXDATA => {
                    self.options.write().auxdata = Self::parse_i32(value)? != 0;
                    Ok(())
                }
                packet_option::PACKET_VERSION => {
                    let v = Self::parse_i32(value)?;
                    let new_version = match v {
                        tpacket_version::TPACKET_V1 => super::ring::TpacketVersion::V1,
                        tpacket_version::TPACKET_V2 => super::ring::TpacketVersion::V2,
                        _ => return Err(SystemError::EINVAL),
                    };
                    let _control = self.ring_control_lock.lock();
                    let mut state = self.ring_state.lock();
                    if state.ring.is_some() {
                        return Err(SystemError::EBUSY);
                    }
                    state.version = new_version;
                    Ok(())
                }
                packet_option::PACKET_RESERVE => {
                    let v = Self::parse_u32_exact(value)?;
                    if v > i32::MAX as u32 {
                        return Err(SystemError::EINVAL);
                    }
                    let _control = self.ring_control_lock.lock();
                    let mut state = self.ring_state.lock();
                    if state.ring.is_some() {
                        return Err(SystemError::EBUSY);
                    }
                    state.reserve = v;
                    Ok(())
                }
                packet_option::PACKET_RX_RING => {
                    if value.len() < 16 {
                        return Err(SystemError::EINVAL);
                    }
                    let req = super::uapi::TpacketReq {
                        tp_block_size: u32::from_ne_bytes(value[0..4].try_into().unwrap()),
                        tp_block_nr: u32::from_ne_bytes(value[4..8].try_into().unwrap()),
                        tp_frame_size: u32::from_ne_bytes(value[8..12].try_into().unwrap()),
                        tp_frame_nr: u32::from_ne_bytes(value[12..16].try_into().unwrap()),
                    };
                    // Match Linux lock_sock(): serialize control-plane changes
                    // while keeping slow allocation and destruction off the
                    // ring_state lock used by ingress and poll.
                    let _control = self.ring_control_lock.lock();
                    // The queue-to-state order is shared with the legacy RX
                    // commit path, making ring cutover and final enqueue one
                    // atomic state transition.
                    let mut queue = self.rx_buffer.lock();
                    let mut state = self.ring_state.lock();
                    // Linux checks mapped before classifying or validating any
                    // setup/teardown request.
                    if state.mapped > 0 {
                        return Err(SystemError::EBUSY);
                    }
                    // A zero block count selects teardown. Only frame_nr must
                    // also be zero; the two size fields are ignored.
                    if req.tp_block_nr == 0 {
                        if req.tp_frame_nr != 0 {
                            return Err(SystemError::EINVAL);
                        }
                        let old_ring = state.ring.take();
                        let old_queue = core::mem::take(&mut *queue);
                        state.advance_rx_generation();
                        drop(state);
                        drop(queue);
                        // Page-cache/ring destruction may be expensive. Arc
                        // snapshots held by in-flight ingress remain valid.
                        self.release_rx_queue(old_queue);
                        drop(old_ring);
                        return Ok(());
                    }
                    if state.ring.is_some() {
                        return Err(SystemError::EBUSY);
                    }
                    let version = state.version;
                    let reserve = state.reserve as usize;
                    drop(state);
                    drop(queue);

                    let config =
                        super::ring::validate_ring_config(&req, version.hdrlen(), reserve)?;
                    let (ring, page_cache) =
                        PacketRing::setup(config, version, self.sock_type, reserve)?;
                    let ring = Arc::try_new(super::ring::PacketRingInstance::new(ring, page_cache))
                        .map_err(|_| SystemError::ENOMEM)?;

                    // The control lock keeps version/reserve/ring stable while
                    // the candidate is built. Publish the fully initialized
                    // ring in one short data-plane-visible critical section.
                    let mut queue = self.rx_buffer.lock();
                    let mut state = self.ring_state.lock();
                    debug_assert!(state.ring.is_none() && state.mapped == 0);
                    let old_queue = core::mem::take(&mut *queue);
                    state.ring = Some(ring);
                    state.advance_rx_generation();
                    drop(state);
                    drop(queue);
                    self.release_rx_queue(old_queue);
                    Ok(())
                }
                packet_option::PACKET_COPY_THRESH => {
                    // PACKET_COPY_THRESH is not implemented in Phase 1. Return
                    // ENOPROTOOPT so userspace knows the option is unavailable
                    // rather than being silently accepted without effect.
                    Err(SystemError::ENOPROTOOPT)
                }
                packet_option::PACKET_FANOUT => {
                    let (raw, max_num_members) = Self::parse_fanout(value)?;
                    self.join_fanout(raw, max_num_members)
                }
                packet_option::PACKET_FANOUT_DATA => Err(SystemError::EINVAL),
                _ => Ok(()),
            },
            // Preserve backward compatibility: unknown levels are silently accepted.
            _ => Ok(()),
        }
    }
    fn set_socket_option(&self, name: usize, val: &[u8]) -> Result<(), SystemError> {
        match PSO::try_from(name as u32) {
            Ok(PSO::RCVBUF) => {
                let size = parse_socket_buffer_size(val, SYSCTL_RMEM_MAX, SOCK_MIN_RCVBUF)?;
                self.recv_buffer_bytes.store(size, Ordering::Relaxed);
                Ok(())
            }
            Ok(PSO::SNDBUF) => {
                let size = parse_socket_buffer_size(val, SYSCTL_WMEM_MAX, SOCK_MIN_SNDBUF)?;
                self.send_buffer_bytes.store(size, Ordering::Relaxed);
                Ok(())
            }
            Ok(PSO::SNDTIMEO_OLD | PSO::SNDTIMEO_NEW | PSO::RCVTIMEO_OLD | PSO::RCVTIMEO_NEW) => {
                let timeout = self.socket_timeout_ticks(name)?;
                let ticks = parse_timeval_ticks(val)?.unwrap_or(INFINITE_TIMEOUT_TICKS);
                timeout.store(ticks, Ordering::Relaxed);
                Ok(())
            }
            Ok(PSO::ATTACH_FILTER) => {
                let fprog = classic::parse_sock_fprog(val)?;
                // Linux checks SOCK_FILTER_LOCKED after copying the fprog
                // header, but before validating or reading its instruction
                // pointer. Keep the final check below to close the race with
                // a concurrent SO_LOCK_FILTER.
                if *self.filter_locked.lock() {
                    return Err(SystemError::EPERM);
                }
                let insns = classic::read_sock_fprog_insns(&fprog)?;
                validate_cbpf(&insns)?;
                let new_filter = Arc::try_new(insns).map_err(|_| SystemError::ENOMEM)?;

                let old_filter = {
                    let locked = self.filter_locked.lock();
                    if *locked {
                        return Err(SystemError::EPERM);
                    }
                    // SAFETY: `old_filter` keeps the removed slot reference
                    // alive across unlock and is submitted to rcu_defer_drop
                    // immediately below.
                    unsafe { self.filter.swap(Some(new_filter)) }
                };
                if let Some(old_filter) = old_filter {
                    rcu_defer_drop(old_filter);
                }
                Ok(())
            }
            Ok(PSO::DETACH_FILTER) => {
                // Linux's SOL_SOCKET path validates and reads an int even
                // though SO_DETACH_FILTER does not use its value.
                let _ = Self::parse_i32(val)?;
                let old_filter = {
                    let locked = self.filter_locked.lock();
                    if *locked {
                        return Err(SystemError::EPERM);
                    }
                    // SAFETY: `old_filter` keeps the removed slot reference
                    // alive across unlock and is submitted to rcu_defer_drop
                    // immediately below.
                    unsafe { self.filter.swap(None) }.ok_or(SystemError::ENOENT)?
                };
                rcu_defer_drop(old_filter);
                Ok(())
            }
            Ok(PSO::LOCK_FILTER) => {
                let lock_filter = Self::parse_i32(val)? != 0;
                let mut locked = self.filter_locked.lock();
                if *locked && !lock_filter {
                    return Err(SystemError::EPERM);
                }
                *locked = lock_filter;
                Ok(())
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }
    fn get_socket_option(&self, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        match PSO::try_from(name as u32) {
            Ok(PSO::RCVBUF) => Ok(write_u32_getsockopt(
                value,
                self.recv_buffer_bytes.load(Ordering::Relaxed) as u32,
            )),
            Ok(PSO::SNDBUF) => Ok(write_u32_getsockopt(
                value,
                self.send_buffer_bytes.load(Ordering::Relaxed) as u32,
            )),
            Ok(PSO::SNDTIMEO_OLD | PSO::SNDTIMEO_NEW | PSO::RCVTIMEO_OLD | PSO::RCVTIMEO_NEW) => {
                let ticks = self.socket_timeout_ticks(name)?.load(Ordering::Relaxed);
                Ok(write_timeval_ticks(value, ticks))
            }
            // SO_GET_FILTER shares its numeric value with SO_ATTACH_FILTER.
            // The getsockopt ABI interprets the supplied buffer length and
            // return value as instruction counts rather than byte counts.
            Ok(PSO::ATTACH_FILTER) => self.get_filter(value),
            Ok(PSO::LOCK_FILTER) => Ok(write_i32_getsockopt(
                value,
                *self.filter_locked.lock() as i32,
            )),
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn get_filter(&self, value: &mut [u8]) -> Result<usize, SystemError> {
        let Some(filter) = self.filter.load() else {
            return Ok(0);
        };
        let count = filter.len();
        if value.is_empty() {
            return Ok(count);
        }

        let insn_size = core::mem::size_of::<SockFilter>();
        if value.len() / insn_size < count {
            return Err(SystemError::EINVAL);
        }

        for (dst, insn) in value.chunks_exact_mut(insn_size).zip(filter.iter()) {
            dst[..2].copy_from_slice(&insn.code.to_ne_bytes());
            dst[2] = insn.jt;
            dst[3] = insn.jf;
            dst[4..8].copy_from_slice(&insn.k.to_ne_bytes());
        }
        Ok(count)
    }
}
