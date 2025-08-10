//! BPF_MAP_TYPE_ARRAY and BPF_MAP_TYPE_PERCPU_ARRAY
//!
//!
//! See https://docs.kernel.org/bpf/map_array.html

use super::super::Result;
use crate::bpf::map::util::round_up;
use crate::bpf::map::{BpfCallBackFn, BpfMapCommonOps, BpfMapMeta};
use crate::mm::percpu::{PerCpu, PerCpuVar};
use crate::smp::cpu::{smp_cpu_manager, ProcessorId};
use alloc::{vec, vec::Vec};
use core::{
    fmt::{Debug, Formatter},
    ops::{Index, IndexMut},
};
use log::info;
use system_error::SystemError;

/// The array map type is a generic map type with no restrictions on the structure of the value.
/// Like a normal array, the array map has a numeric key starting at 0 and incrementing.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/map-type/BPF_MAP_TYPE_ARRAY/
#[derive(Debug)]
pub struct ArrayMap {
    max_entries: u32,
    data: ArrayMapData,
}

struct ArrayMapData {
    elem_size: u32,
    /// The data is stored in a Vec<u8> with the size of elem_size * max_entries.
    data: Vec<u8>,
}

impl Debug for ArrayMapData {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ArrayMapData")
            .field("elem_size", &self.elem_size)
            .field("data_len", &self.data.len())
            .finish()
    }
}

impl ArrayMapData {
    pub fn new(elem_size: u32, max_entries: u32) -> Self {
        debug_assert!(elem_size % 8 == 0);
        let total_size = elem_size * max_entries;
        let data = vec![0; total_size as usize];
        ArrayMapData { elem_size, data }
    }
}

impl Index<u32> for ArrayMapData {
    type Output = [u8];
    fn index(&self, index: u32) -> &Self::Output {
        let start = index * self.elem_size;
        &self.data[start as usize..(start + self.elem_size) as usize]
    }
}

impl IndexMut<u32> for ArrayMapData {
    fn index_mut(&mut self, index: u32) -> &mut Self::Output {
        let start = index * self.elem_size;
        &mut self.data[start as usize..(start + self.elem_size) as usize]
    }
}

impl ArrayMap {
    pub fn new(attr: &BpfMapMeta) -> Result<Self> {
        if attr.value_size == 0 || attr.max_entries == 0 || attr.key_size != 4 {
            return Err(SystemError::EINVAL);
        }
        let elem_size = round_up(attr.value_size as usize, 8);
        let data = ArrayMapData::new(elem_size as u32, attr.max_entries);
        Ok(ArrayMap {
            max_entries: attr.max_entries,
            data,
        })
    }
}

impl BpfMapCommonOps for ArrayMap {
    fn lookup_elem(&mut self, key: &[u8]) -> Result<Option<&[u8]>> {
        if key.len() != 4 {
            return Err(SystemError::EINVAL);
        }
        let index = u32::from_ne_bytes(key.try_into().map_err(|_| SystemError::EINVAL)?);
        if index >= self.max_entries {
            return Err(SystemError::EINVAL);
        }
        let val = self.data.index(index);
        Ok(Some(val))
    }
    fn update_elem(&mut self, key: &[u8], value: &[u8], _flags: u64) -> Result<()> {
        if key.len() != 4 {
            return Err(SystemError::EINVAL);
        }
        let index = u32::from_ne_bytes(key.try_into().map_err(|_| SystemError::EINVAL)?);
        if index >= self.max_entries {
            return Err(SystemError::EINVAL);
        }
        if value.len() > self.data.elem_size as usize {
            return Err(SystemError::EINVAL);
        }
        let old_value = self.data.index_mut(index);
        old_value[..value.len()].copy_from_slice(value);
        Ok(())
    }
    /// For ArrayMap, delete_elem is not supported.
    fn delete_elem(&mut self, _key: &[u8]) -> Result<()> {
        Err(SystemError::EINVAL)
    }
    fn for_each_elem(&mut self, cb: BpfCallBackFn, ctx: *const u8, flags: u64) -> Result<u32> {
        if flags != 0 {
            return Err(SystemError::EINVAL);
        }
        let mut total_used = 0;
        for i in 0..self.max_entries {
            let key = i.to_ne_bytes();
            let value = self.data.index(i);
            total_used += 1;
            let res = cb(&key, value, ctx);
            // return value: 0 - continue, 1 - stop and return
            if res != 0 {
                break;
            }
        }
        Ok(total_used)
    }

    fn lookup_and_delete_elem(&mut self, _key: &[u8], _value: &mut [u8]) -> Result<()> {
        Err(SystemError::EINVAL)
    }

    fn get_next_key(&self, key: Option<&[u8]>, next_key: &mut [u8]) -> Result<()> {
        if let Some(key) = key {
            if key.len() != 4 {
                return Err(SystemError::EINVAL);
            }
            let index = u32::from_ne_bytes(key.try_into().map_err(|_| SystemError::EINVAL)?);
            if index == self.max_entries - 1 {
                return Err(SystemError::ENOENT);
            }
            let next_index = index + 1;
            next_key.copy_from_slice(&next_index.to_ne_bytes());
        } else {
            next_key.copy_from_slice(&0u32.to_ne_bytes());
        }
        Ok(())
    }

    fn freeze(&self) -> Result<()> {
        info!("fake freeze done for ArrayMap");
        Ok(())
    }
    fn first_value_ptr(&self) -> Result<*const u8> {
        Ok(self.data.data.as_ptr())
    }
}

/// This is the per-CPU variant of the [ArrayMap] map type.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/map-type/BPF_MAP_TYPE_PERCPU_ARRAY/
pub struct PerCpuArrayMap {
    per_cpu_data: PerCpuVar<ArrayMap>,
}

impl Debug for PerCpuArrayMap {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PerCpuArrayMap")
            .field("data", &self.per_cpu_data)
            .finish()
    }
}

impl PerCpuArrayMap {
    pub fn new(attr: &BpfMapMeta) -> Result<Self> {
        let num_cpus = PerCpu::MAX_CPU_NUM;
        let mut data = Vec::with_capacity(num_cpus as usize);
        for _ in 0..num_cpus {
            let array_map = ArrayMap::new(attr)?;
            data.push(array_map);
        }
        let per_cpu_data = PerCpuVar::new(data).ok_or(SystemError::EINVAL)?;
        Ok(PerCpuArrayMap { per_cpu_data })
    }
}

impl BpfMapCommonOps for PerCpuArrayMap {
    fn lookup_elem(&mut self, key: &[u8]) -> Result<Option<&[u8]>> {
        self.per_cpu_data.get_mut().lookup_elem(key)
    }
    fn update_elem(&mut self, key: &[u8], value: &[u8], flags: u64) -> Result<()> {
        self.per_cpu_data.get_mut().update_elem(key, value, flags)
    }
    fn delete_elem(&mut self, key: &[u8]) -> Result<()> {
        self.per_cpu_data.get_mut().delete_elem(key)
    }
    fn for_each_elem(&mut self, cb: BpfCallBackFn, ctx: *const u8, flags: u64) -> Result<u32> {
        self.per_cpu_data.get_mut().for_each_elem(cb, ctx, flags)
    }
    fn lookup_and_delete_elem(&mut self, _key: &[u8], _value: &mut [u8]) -> Result<()> {
        Err(SystemError::EINVAL)
    }
    fn lookup_percpu_elem(&mut self, key: &[u8], cpu: u32) -> Result<Option<&[u8]>> {
        unsafe {
            self.per_cpu_data
                .force_get_mut(ProcessorId::new(cpu))
                .lookup_elem(key)
        }
    }
    fn get_next_key(&self, key: Option<&[u8]>, next_key: &mut [u8]) -> Result<()> {
        self.per_cpu_data.get_mut().get_next_key(key, next_key)
    }
    fn first_value_ptr(&self) -> Result<*const u8> {
        self.per_cpu_data.get_mut().first_value_ptr()
    }
}

/// See https://ebpf-docs.dylanreimerink.nl/linux/map-type/BPF_MAP_TYPE_PERF_EVENT_ARRAY/
pub struct PerfEventArrayMap {
    // The value is the file descriptor of the perf event.
    fds: ArrayMapData,
}

impl Debug for PerfEventArrayMap {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PerfEventArrayMap")
            .field("fds", &self.fds)
            .finish()
    }
}

impl PerfEventArrayMap {
    pub fn new(attr: &BpfMapMeta) -> Result<Self> {
        let num_cpus = smp_cpu_manager().possible_cpus_count();
        if attr.key_size != 4 || attr.value_size != 4 || attr.max_entries != num_cpus {
            return Err(SystemError::EINVAL);
        }
        let fds = ArrayMapData::new(4, num_cpus);
        Ok(PerfEventArrayMap { fds })
    }
}

impl BpfMapCommonOps for PerfEventArrayMap {
    fn lookup_elem(&mut self, key: &[u8]) -> Result<Option<&[u8]>> {
        let cpu_id = u32::from_ne_bytes(key.try_into().map_err(|_| SystemError::EINVAL)?);
        let value = self.fds.index(cpu_id);
        Ok(Some(value))
    }
    fn update_elem(&mut self, key: &[u8], value: &[u8], _flags: u64) -> Result<()> {
        assert_eq!(value.len(), 4);
        let cpu_id = u32::from_ne_bytes(key.try_into().map_err(|_| SystemError::EINVAL)?);
        let old_value = self.fds.index_mut(cpu_id);
        old_value.copy_from_slice(value);
        Ok(())
    }
    fn delete_elem(&mut self, key: &[u8]) -> Result<()> {
        let cpu_id = u32::from_ne_bytes(key.try_into().map_err(|_| SystemError::EINVAL)?);
        self.fds.index_mut(cpu_id).copy_from_slice(&[0; 4]);
        Ok(())
    }
    fn for_each_elem(&mut self, cb: BpfCallBackFn, ctx: *const u8, _flags: u64) -> Result<u32> {
        let mut total_used = 0;
        let num_cpus = smp_cpu_manager().possible_cpus_count();
        for i in 0..num_cpus {
            let key = i.to_ne_bytes();
            let value = self.fds.index(i);
            total_used += 1;
            let res = cb(&key, value, ctx);
            if res != 0 {
                break;
            }
        }
        Ok(total_used)
    }
    fn lookup_and_delete_elem(&mut self, _key: &[u8], _value: &mut [u8]) -> Result<()> {
        Err(SystemError::EINVAL)
    }
    fn first_value_ptr(&self) -> Result<*const u8> {
        Ok(self.fds.data.as_ptr())
    }
}
