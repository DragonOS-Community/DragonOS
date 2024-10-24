use super::{BpfCallBackFn, BpfMapCommonOps, Result};
use crate::bpf::map::util::BpfMapMeta;
use crate::mm::percpu::{PerCpu, PerCpuVar};
use crate::smp::cpu::ProcessorId;
use alloc::vec::Vec;
use core::fmt::Debug;
use core::num::NonZero;
use lru::LruCache;
use system_error::SystemError;

type BpfHashMapKey = Vec<u8>;
type BpfHashMapValue = Vec<u8>;
/// This map is the LRU (Least Recently Used) variant of the BPF_MAP_TYPE_HASH.
/// It is a generic map type that stores a fixed maximum number of key/value pairs.
/// When the map starts to get at capacity, the approximately least recently
/// used elements is removed to make room for new elements.
///
/// See https://docs.ebpf.io/linux/map-type/BPF_MAP_TYPE_LRU_HASH/
#[derive(Debug)]
pub struct LruMap {
    _max_entries: u32,
    data: LruCache<BpfHashMapKey, BpfHashMapValue>,
}

impl LruMap {
    pub fn new(attr: &BpfMapMeta) -> Result<Self> {
        if attr.value_size == 0 || attr.max_entries == 0 {
            return Err(SystemError::EINVAL);
        }
        Ok(Self {
            _max_entries: attr.max_entries,
            data: LruCache::new(
                NonZero::new(attr.max_entries as usize).ok_or(SystemError::EINVAL)?,
            ),
        })
    }
}

impl BpfMapCommonOps for LruMap {
    fn lookup_elem(&mut self, key: &[u8]) -> Result<Option<&[u8]>> {
        let value = self.data.get(key).map(|v| v.as_slice());
        Ok(value)
    }
    fn update_elem(&mut self, key: &[u8], value: &[u8], _flags: u64) -> Result<()> {
        self.data.put(key.to_vec(), value.to_vec());
        Ok(())
    }
    fn delete_elem(&mut self, key: &[u8]) -> Result<()> {
        self.data.pop(key);
        Ok(())
    }
    fn for_each_elem(&mut self, cb: BpfCallBackFn, ctx: *const u8, flags: u64) -> Result<u32> {
        if flags != 0 {
            return Err(SystemError::EINVAL);
        }
        let mut total_used = 0;
        for (key, value) in self.data.iter() {
            let res = cb(key, value, ctx);
            // return value: 0 - continue, 1 - stop and return
            if res != 0 {
                break;
            }
            total_used += 1;
        }
        Ok(total_used)
    }
    fn lookup_and_delete_elem(&mut self, key: &[u8], value: &mut [u8]) -> Result<()> {
        let v = self
            .data
            .get(key)
            .map(|v| v.as_slice())
            .ok_or(SystemError::ENOENT)?;
        value.copy_from_slice(v);
        self.data.pop(key);
        Ok(())
    }
    fn get_next_key(&self, key: Option<&[u8]>, next_key: &mut [u8]) -> Result<()> {
        let mut iter = self.data.iter();
        if let Some(key) = key {
            for (k, _) in iter.by_ref() {
                if k.as_slice() == key {
                    break;
                }
            }
        }
        let res = iter.next();
        match res {
            Some((k, _)) => {
                next_key.copy_from_slice(k.as_slice());
                Ok(())
            }
            None => Err(SystemError::ENOENT),
        }
    }
}

/// See https://ebpf-docs.dylanreimerink.nl/linux/map-type/BPF_MAP_TYPE_LRU_PERCPU_HASH/
pub struct PerCpuLruMap {
    per_cpu_maps: PerCpuVar<LruMap>,
}

impl Debug for PerCpuLruMap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PerCpuLruMap")
            .field("maps", &self.per_cpu_maps)
            .finish()
    }
}

impl PerCpuLruMap {
    pub fn new(attr: &BpfMapMeta) -> Result<Self> {
        let num_cpus = PerCpu::MAX_CPU_NUM;
        let mut data = Vec::with_capacity(num_cpus as usize);
        for _ in 0..num_cpus {
            let array_map = LruMap::new(attr)?;
            data.push(array_map);
        }
        let per_cpu_maps = PerCpuVar::new(data).ok_or(SystemError::EINVAL)?;
        Ok(PerCpuLruMap { per_cpu_maps })
    }
}

impl BpfMapCommonOps for PerCpuLruMap {
    fn lookup_elem(&mut self, key: &[u8]) -> Result<Option<&[u8]>> {
        self.per_cpu_maps.get_mut().lookup_elem(key)
    }
    fn update_elem(&mut self, key: &[u8], value: &[u8], flags: u64) -> Result<()> {
        self.per_cpu_maps.get_mut().update_elem(key, value, flags)
    }
    fn delete_elem(&mut self, key: &[u8]) -> Result<()> {
        self.per_cpu_maps.get_mut().delete_elem(key)
    }
    fn for_each_elem(&mut self, cb: BpfCallBackFn, ctx: *const u8, flags: u64) -> Result<u32> {
        self.per_cpu_maps.get_mut().for_each_elem(cb, ctx, flags)
    }
    fn lookup_and_delete_elem(&mut self, key: &[u8], value: &mut [u8]) -> Result<()> {
        self.per_cpu_maps
            .get_mut()
            .lookup_and_delete_elem(key, value)
    }
    fn lookup_percpu_elem(&mut self, key: &[u8], cpu: u32) -> Result<Option<&[u8]>> {
        unsafe {
            self.per_cpu_maps
                .force_get_mut(ProcessorId::new(cpu))
                .lookup_elem(key)
        }
    }
    fn get_next_key(&self, key: Option<&[u8]>, next_key: &mut [u8]) -> Result<()> {
        self.per_cpu_maps.get_mut().get_next_key(key, next_key)
    }
}
