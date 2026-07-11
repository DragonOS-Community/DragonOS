use core::sync::atomic::Ordering;
use system_error::SystemError;

use crate::net::socket::common::write_i32_getsockopt;
use crate::net::socket::common::{parse_timeval_opt, write_timeval_opt};
use crate::net::socket::{PSO, PSOL};

use super::{packet_option, PacketSocket};

impl PacketSocket {
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
        match PSO::try_from(name as u32) {
            Ok(PSO::RCVTIMEO_OLD) | Ok(PSO::RCVTIMEO_NEW) => {
                let d = parse_timeval_opt(val)?;
                let us = d.map(|v| v.total_micros()).unwrap_or(u64::MAX);
                self.recv_timeout_us.store(us, Ordering::Relaxed);
                Ok(())
            }
            // Preserve backward compatibility: unknown SOL_SOCKET options are silently accepted.
            _ => Ok(()),
        }
    }
    fn get_socket_option(&self, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        match PSO::try_from(name as u32) {
            Ok(PSO::RCVTIMEO_OLD) | Ok(PSO::RCVTIMEO_NEW) => {
                let us = self.recv_timeout_us.load(Ordering::Relaxed);
                let us = if us == u64::MAX { 0 } else { us };
                Ok(write_timeval_opt(value, us))
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }
}
