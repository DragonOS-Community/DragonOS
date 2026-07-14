use core::sync::atomic::{AtomicU64, Ordering};
use system_error::SystemError;

use crate::net::socket::common::{
    parse_socket_buffer_size, parse_timeval_ticks, write_i32_getsockopt, write_timeval_ticks,
    write_u32_getsockopt, INFINITE_TIMEOUT_TICKS, SOCK_MIN_RCVBUF, SOCK_MIN_SNDBUF,
    SYSCTL_RMEM_MAX, SYSCTL_WMEM_MAX,
};
use crate::net::socket::{PSO, PSOL};

use super::ring::PacketRing;
use super::uapi::tpacket_version;
use super::{packet_option, PacketSocket};
use crate::libs::mutex::Mutex;
use alloc::sync::Arc;

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
                    let packets = self.stats_packets.swap(0, Ordering::Relaxed);
                    let drops = self.stats_drops.swap(0, Ordering::Relaxed);
                    value[..4].copy_from_slice(&packets.to_ne_bytes());
                    value[4..8].copy_from_slice(&drops.to_ne_bytes());
                    Ok(8)
                }
                packet_option::PACKET_AUXDATA => Ok(write_i32_getsockopt(
                    value,
                    self.options.read().auxdata as i32,
                )),
                packet_option::PACKET_VERSION => {
                    let v = match *self.tpacket_version.lock() {
                        super::TpacketVersion::V1 => tpacket_version::TPACKET_V1,
                        super::TpacketVersion::V2 => tpacket_version::TPACKET_V2,
                    };
                    Ok(write_i32_getsockopt(value, v))
                }
                packet_option::PACKET_HDRLEN => {
                    let hdrlen = self.tpacket_version.lock().hdrlen() as i32;
                    Ok(write_i32_getsockopt(value, hdrlen))
                }
                packet_option::PACKET_RESERVE => {
                    Ok(write_i32_getsockopt(value, self.tp_reserve.load(Ordering::Relaxed) as i32))
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
                packet_option::PACKET_ADD_MEMBERSHIP | packet_option::PACKET_DROP_MEMBERSHIP => {
                    Ok(())
                }
                packet_option::PACKET_AUXDATA => {
                    self.options.write().auxdata = Self::parse_i32(value)? != 0;
                    Ok(())
                }
                packet_option::PACKET_VERSION => {
                    let v = Self::parse_i32(value)?;
                    let new_version = match v {
                        tpacket_version::TPACKET_V1 => super::TpacketVersion::V1,
                        tpacket_version::TPACKET_V2 => super::TpacketVersion::V2,
                        _ => return Err(SystemError::EINVAL),
                    };
                    if self.rx_ring.lock().is_some() {
                        return Err(SystemError::EBUSY);
                    }
                    *self.tpacket_version.lock() = new_version;
                    Ok(())
                }
                packet_option::PACKET_RESERVE => {
                    let v = Self::parse_i32(value)? as u32;
                    if v > 255 {
                        return Err(SystemError::EINVAL);
                    }
                    if self.rx_ring.lock().is_some() {
                        return Err(SystemError::EBUSY);
                    }
                    self.tp_reserve.store(v, Ordering::Relaxed);
                    Ok(())
                }
                packet_option::PACKET_RX_RING => {
                    if value.len() < 16 {
                        return Err(SystemError::EINVAL);
                    }
                    if self.rx_ring.lock().is_some() {
                        return Err(SystemError::EBUSY);
                    }
                    let req = super::uapi::TpacketReq {
                        tp_block_size: u32::from_ne_bytes(value[0..4].try_into().unwrap()),
                        tp_block_nr: u32::from_ne_bytes(value[4..8].try_into().unwrap()),
                        tp_frame_size: u32::from_ne_bytes(value[8..12].try_into().unwrap()),
                        tp_frame_nr: u32::from_ne_bytes(value[12..16].try_into().unwrap()),
                    };
                    let version = *self.tpacket_version.lock();
                    let reserve = self.tp_reserve.load(Ordering::Relaxed) as usize;
                    let config =
                        super::ring::validate_ring_config(&req, version.hdrlen(), reserve)?;
                    let (ring, _pc) = PacketRing::setup(config, version, self.sock_type, reserve)?;
                    *self.rx_ring.lock() = Some(Arc::new(Mutex::new(ring)));
                    // Flush stale packets queued before ring setup (review P2 fix).
                    // Linux calls __skb_queue_purge in packet_set_ring. Without this,
                    // packets received between bind() and ring setup are stranded —
                    // can_recv() only checks the ring when it is active.
                    let mut q = self.rx_buffer.lock();
                    let bytes: usize = q.iter().map(|p| p.accounted_bytes).sum();
                    q.clear();
                    drop(q);
                    self.rx_buffer_bytes.fetch_sub(bytes, Ordering::AcqRel);
                    Ok(())
                }
                packet_option::PACKET_COPY_THRESH => {
                    let _ = Self::parse_i32(value)?;
                    Ok(())
                }
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
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }
}
