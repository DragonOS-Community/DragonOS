use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use system_error::SystemError;

use crate::bpf::classic::{self, validate_cbpf};
use crate::net::socket::common::{
    parse_socket_buffer_size, parse_timeval_ticks, write_i32_getsockopt, write_timeval_ticks,
    write_u32_getsockopt, INFINITE_TIMEOUT_TICKS, SOCK_MIN_RCVBUF, SOCK_MIN_SNDBUF,
    SYSCTL_RMEM_MAX, SYSCTL_WMEM_MAX,
};
use crate::net::socket::{PSO, PSOL};
use crate::rcu::rcu_defer_drop;

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
                    self.filter.swap(Some(new_filter))
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
                    self.filter.swap(None).ok_or(SystemError::ENOENT)?
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
            Ok(PSO::LOCK_FILTER) => Ok(write_i32_getsockopt(
                value,
                *self.filter_locked.lock() as i32,
            )),
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }
}
