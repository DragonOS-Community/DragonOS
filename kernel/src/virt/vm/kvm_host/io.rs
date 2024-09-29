use core::fmt::Debug;

use alloc::vec::Vec;

use alloc::boxed::Box;
use system_error::SystemError;

use super::vcpu::VirtCpu;
use super::{KvmBus, Vm};

pub trait KvmIoDeviceOps: Send + Sync + Debug {
    fn read(
        &self,
        vcpu: &VirtCpu,
        addr: usize,
        len: u32,
        val: &mut usize,
    ) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn write(&self, vcpu: &VirtCpu, addr: usize, len: u32, val: &usize) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
}

#[derive(Debug)]
pub struct KvmIoRange {
    pub addr: usize,
    pub len: u32,
    pub dev_ops: Option<Box<dyn KvmIoDeviceOps>>,
}

impl PartialEq for KvmIoRange {
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr && self.len == other.len
    }
}

impl Eq for KvmIoRange {}

impl PartialOrd for KvmIoRange {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        let mut addr1 = self.addr;
        let mut addr2 = other.addr;

        if addr1 < addr2 {
            return Some(core::cmp::Ordering::Less);
        }

        if other.len != 0 {
            addr1 += self.len as usize;
            addr2 += other.len as usize;
        }

        if addr1 > addr2 {
            return Some(core::cmp::Ordering::Greater);
        }

        return Some(core::cmp::Ordering::Equal);
    }
}

impl Ord for KvmIoRange {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        return self.partial_cmp(other).unwrap();
    }
}

#[derive(Debug)]
pub struct KvmIoBus {
    pub dev_count: u32,
    pub ioeventfd_count: u32,
    pub range: Vec<KvmIoRange>,
}

impl VirtCpu {
    pub fn kvm_io_bus_write(
        &self,
        vm: &mut Vm,
        bus_idx: KvmBus,
        addr: usize,
        len: u32,
        val: &usize,
    ) -> Result<(), SystemError> {
        let bus_idx = bus_idx as usize;
        if bus_idx >= vm.buses.len() {
            return Err(SystemError::ENOMEM);
        }
        let bus = &mut vm.buses[bus_idx];
        let range = KvmIoRange {
            addr,
            len,
            dev_ops: None,
        };

        return self.internal_kvm_bus_write(bus, range, val).and(Ok(()));
    }

    fn internal_kvm_bus_write(
        &self,
        bus: &KvmIoBus,
        range: KvmIoRange,
        val: &usize,
    ) -> Result<usize, SystemError> {
        let mut idx = Self::kvm_io_bus_get_first_dev(bus, range.addr, range.len)?;

        while idx < bus.dev_count as usize && range == bus.range[idx] {
            if let Some(dev_ops) = &bus.range[idx].dev_ops {
                dev_ops.write(self, range.addr, range.len, val)?;
                return Ok(idx);
            }
            idx += 1;
        }

        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    pub fn kvm_io_bus_read(
        &self,
        vm: &mut Vm,
        bus_idx: KvmBus,
        addr: usize,
        len: u32,
        val: &mut usize,
    ) -> Result<(), SystemError> {
        let bus_idx = bus_idx as usize;
        if bus_idx >= vm.buses.len() {
            return Err(SystemError::ENOMEM);
        }
        let bus = &mut vm.buses[bus_idx];
        let range = KvmIoRange {
            addr,
            len,
            dev_ops: None,
        };

        return self.internal_kvm_bus_read(bus, range, val).and(Ok(()));
    }

    fn internal_kvm_bus_read(
        &self,
        bus: &KvmIoBus,
        range: KvmIoRange,
        val: &mut usize,
    ) -> Result<usize, SystemError> {
        let mut idx = Self::kvm_io_bus_get_first_dev(bus, range.addr, range.len)?;

        while idx < bus.dev_count as usize && range == bus.range[idx] {
            if let Some(dev_ops) = &bus.range[idx].dev_ops {
                dev_ops.read(self, range.addr, range.len, val)?;
                return Ok(idx);
            }
            idx += 1;
        }

        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn kvm_io_bus_get_first_dev(
        bus: &KvmIoBus,
        addr: usize,
        len: u32,
    ) -> Result<usize, SystemError> {
        let key = KvmIoRange {
            addr,
            len,
            dev_ops: None,
        };
        let range = bus.range.binary_search(&key);

        if let Ok(mut idx) = range {
            while idx > 0 && key == bus.range[idx - 1] {
                idx -= 1;
            }

            return Ok(idx);
        } else {
            return Err(SystemError::ENOENT);
        }
    }
}
