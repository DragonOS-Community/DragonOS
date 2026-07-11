use core::sync::atomic::Ordering;
use system_error::SystemError;

use crate::net::socket::common::write_i32_getsockopt;
use crate::net::socket::PSOL;

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
        if level != PSOL::PACKET {
            return Err(SystemError::ENOPROTOOPT);
        }
        match name {
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
        }
    }
    pub(super) fn set_packet_option(
        &self,
        level: PSOL,
        name: usize,
        value: &[u8],
    ) -> Result<(), SystemError> {
        // Preserve the base compatibility contract for non-PACKET and unsupported names.
        if level != PSOL::PACKET {
            return Ok(());
        }
        match name {
            packet_option::PACKET_ADD_MEMBERSHIP | packet_option::PACKET_DROP_MEMBERSHIP => Ok(()),
            packet_option::PACKET_AUXDATA => {
                self.options.write().auxdata = Self::parse_i32(value)? != 0;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
