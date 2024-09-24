use super::{BpfCallBackFn, BpfMapCommonOps, PerCpuInfo, Result};
use crate::bpf::map::util::{round_up, BpfMapMeta};
use crate::libs::spinlock::SpinLock;
use alloc::vec::Vec;
use core::fmt::Debug;
use core::num::NonZero;
use lru::LruCache;
use system_error::SystemError;

type BpfHashMapKey = Vec<u8>;
type BpfHashMapValue = Vec<u8>;

#[derive(Debug)]
pub struct LruMap {
    max_entries: u32,
    data: LruCache<BpfHashMapKey, BpfHashMapValue>,
}

impl LruMap {
    pub fn new(attr: &BpfMapMeta) -> Result<Self> {
        if attr.value_size == 0 || attr.max_entries == 0 {
            return Err(SystemError::EINVAL);
        }
        let value_size = round_up(attr.value_size as usize, 8);
        Ok(Self {
            max_entries: attr.max_entries,
            data: LruCache::new(NonZero::new(attr.max_entries as usize).unwrap()),
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
    maps: Vec<LruMap>,
}

impl Debug for PerCpuLruMap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PerCpuLruMap")
            .field("maps", &self.maps)
            .finish()
    }
}

impl PerCpuLruMap {
    pub fn new(attr: &BpfMapMeta) -> Result<Self> {
        let num_cpus = PerCpuInfo::num_cpus();
        let mut data = Vec::with_capacity(num_cpus as usize);
        for _ in 0..num_cpus {
            let array_map = LruMap::new(attr)?;
            data.push(array_map);
        }
        Ok(PerCpuLruMap { maps: data })
    }
}

impl BpfMapCommonOps for PerCpuLruMap {
    fn lookup_elem(&mut self, key: &[u8]) -> Result<Option<&[u8]>> {
        self.maps[PerCpuInfo::cpu_id() as usize].lookup_elem(key)
    }
    fn update_elem(&mut self, key: &[u8], value: &[u8], flags: u64) -> Result<()> {
        self.maps[PerCpuInfo::cpu_id() as usize].update_elem(key, value, flags)
    }
    fn delete_elem(&mut self, key: &[u8]) -> Result<()> {
        self.maps[PerCpuInfo::cpu_id() as usize].delete_elem(key)
    }
    fn for_each_elem(&mut self, cb: BpfCallBackFn, ctx: *const u8, flags: u64) -> Result<u32> {
        self.maps[PerCpuInfo::cpu_id() as usize].for_each_elem(cb, ctx, flags)
    }
    fn lookup_and_delete_elem(&mut self, key: &[u8], value: &mut [u8]) -> Result<()> {
        self.maps[PerCpuInfo::cpu_id() as usize].lookup_and_delete_elem(key, value)
    }
    fn lookup_percpu_elem(&mut self, key: &[u8], cpu: u32) -> Result<Option<&[u8]>> {
        self.maps[cpu as usize].lookup_elem(key)
    }
    fn get_next_key(&self, key: Option<&[u8]>, next_key: &mut [u8]) -> Result<()> {
        self.maps[PerCpuInfo::cpu_id() as usize].get_next_key(key, next_key)
    }
}
