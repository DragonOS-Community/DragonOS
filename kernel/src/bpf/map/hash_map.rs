use super::Result;
use crate::bpf::map::util::{round_up, BpfMapUpdateElemFlags};
use crate::bpf::map::{BpfCallBackFn, BpfMapCommonOps, BpfMapMeta};
use crate::mm::percpu::{PerCpu, PerCpuVar};
use crate::smp::cpu::ProcessorId;
use alloc::{collections::BTreeMap, vec::Vec};
use core::fmt::Debug;
use system_error::SystemError;

type BpfHashMapKey = Vec<u8>;
type BpfHashMapValue = Vec<u8>;

/// The hash map type is a generic map type with no restrictions on the structure of the key and value.
/// Hash-maps are implemented using a hash table, allowing for lookups with arbitrary keys.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/map-type/BPF_MAP_TYPE_HASH/
#[derive(Debug)]
pub struct BpfHashMap {
    _max_entries: u32,
    _key_size: u32,
    _value_size: u32,
    data: BTreeMap<BpfHashMapKey, BpfHashMapValue>,
}

impl BpfHashMap {
    pub fn new(attr: &BpfMapMeta) -> Result<Self> {
        if attr.value_size == 0 || attr.max_entries == 0 {
            return Err(SystemError::EINVAL);
        }
        let value_size = round_up(attr.value_size as usize, 8);
        Ok(Self {
            _max_entries: attr.max_entries,
            _key_size: attr.key_size,
            _value_size: value_size as u32,
            data: BTreeMap::new(),
        })
    }
}

impl BpfMapCommonOps for BpfHashMap {
    fn lookup_elem(&mut self, key: &[u8]) -> Result<Option<&[u8]>> {
        let value = self.data.get(key).map(|v| v.as_slice());
        Ok(value)
    }
    fn update_elem(&mut self, key: &[u8], value: &[u8], flags: u64) -> Result<()> {
        let _flags = BpfMapUpdateElemFlags::from_bits_truncate(flags);
        self.data.insert(key.to_vec(), value.to_vec());
        Ok(())
    }
    fn delete_elem(&mut self, key: &[u8]) -> Result<()> {
        self.data.remove(key);
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
        self.data.remove(key);
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

/// This is the per-CPU variant of the [BpfHashMap] map type.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/map-type/BPF_MAP_TYPE_PERCPU_HASH/
pub struct PerCpuHashMap {
    per_cpu_maps: PerCpuVar<BpfHashMap>,
}

impl Debug for PerCpuHashMap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PerCpuHashMap")
            .field("maps", &self.per_cpu_maps)
            .finish()
    }
}
impl PerCpuHashMap {
    pub fn new(attr: &BpfMapMeta) -> Result<Self> {
        let num_cpus = PerCpu::MAX_CPU_NUM;
        let mut data = Vec::with_capacity(num_cpus as usize);
        for _ in 0..num_cpus {
            let array_map = BpfHashMap::new(attr)?;
            data.push(array_map);
        }
        let per_cpu_maps = PerCpuVar::new(data).ok_or(SystemError::EINVAL)?;
        Ok(PerCpuHashMap { per_cpu_maps })
    }
}
impl BpfMapCommonOps for PerCpuHashMap {
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
    fn first_value_ptr(&self) -> Result<*const u8> {
        self.per_cpu_maps.get_mut().first_value_ptr()
    }
}
