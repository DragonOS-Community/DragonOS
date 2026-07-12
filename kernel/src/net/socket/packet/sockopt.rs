use core::sync::atomic::{AtomicU64, Ordering};
use system_error::SystemError;

use crate::net::socket::common::write_i32_getsockopt;
use crate::net::socket::common::{
    parse_timeval_ticks, write_timeval_ticks, INFINITE_TIMEOUT_TICKS,
};
use crate::net::socket::{PSO, PSOL};

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
                    let accepted = self.stats_packets.swap(0, Ordering::Relaxed);
                    let drops = self.stats_drops.swap(0, Ordering::Relaxed);
                    value[..4].copy_from_slice(&accepted.wrapping_add(drops).to_ne_bytes());
                    value[4..8].copy_from_slice(&drops.to_ne_bytes());
                    Ok(8)
                }
                packet_option::PACKET_AUXDATA => Ok(write_i32_getsockopt(
                    value,
                    self.options.read().auxdata as i32,
                )),
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
                _ => Ok(()),
            },
            // Preserve backward compatibility: unknown levels are silently accepted.
            _ => Ok(()),
        }
    }
    fn set_socket_option(&self, name: usize, val: &[u8]) -> Result<(), SystemError> {
        let timeout = self.socket_timeout_ticks(name)?;
        let ticks = parse_timeval_ticks(val)?.unwrap_or(INFINITE_TIMEOUT_TICKS);
        timeout.store(ticks, Ordering::Relaxed);
        Ok(())
    }
    fn get_socket_option(&self, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        let ticks = self.socket_timeout_ticks(name)?.load(Ordering::Relaxed);
        Ok(write_timeval_ticks(value, ticks))
    }
}
