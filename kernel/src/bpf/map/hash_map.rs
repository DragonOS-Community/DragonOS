use crate::bpf::map::util::round_up;
use crate::bpf::map::{BpfCallBackFn, BpfMapCommonOps, BpfMapMeta};
use alloc::{collections::BTreeMap, vec::Vec};
use system_error::SystemError;

type BpfHashMapKey = Vec<u8>;
type BpfHashMapValue = Vec<u8>;

#[derive(Debug)]
pub struct BpfHashMap {
    max_entries: u32,
    key_size: u32,
    value_size: u32,
    data: BTreeMap<BpfHashMapKey, BpfHashMapValue>,
}

impl TryFrom<&BpfMapMeta> for BpfHashMap {
    type Error = SystemError;
    fn try_from(attr: &BpfMapMeta) -> Result<Self, Self::Error> {
        if attr.value_size == 0 || attr.max_entries == 0 {
            return Err(SystemError::EINVAL);
        }
        let value_size = round_up(attr.value_size as usize, 8);
        Ok(Self {
            max_entries: attr.max_entries,
            key_size: attr.key_size,
            value_size: value_size as u32,
            data: BTreeMap::new(),
        })
    }
}

impl BpfMapCommonOps for BpfHashMap {
    fn lookup_elem(&self, key: &[u8]) -> super::Result<Option<&[u8]>> {
        let value = self.data.get(key).map(|v| v.as_slice());
        Ok(value)
    }
    fn update_elem(&mut self, key: &[u8], value: &[u8], _flags: u64) -> super::Result<()> {
        self.data.insert(key.to_vec(), value.to_vec());
        Ok(())
    }
    fn delete_elem(&mut self, key: &[u8]) -> super::Result<()> {
        self.data.remove(key);
        Ok(())
    }
    fn for_each_elem(&mut self, cb: BpfCallBackFn, ctx: &[u8], flags: u64) -> super::Result<u32> {
        if flags != 0 {
            return Err(SystemError::EINVAL);
        }
        let mut total_used = 0;
        for (key, value) in self.data.iter() {
            let res = cb(ctx, key, value);
            // return value: 0 - continue, 1 - stop and return
            if res != 0 {
                break;
            }
            total_used += 1;
        }
        Ok(total_used)
    }
    fn get_next_key(&self, key: Option<&[u8]>, next_key: &mut [u8]) -> crate::bpf::Result<()> {
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
    fn freeze(&self) -> super::Result<()> {
        Ok(())
    }
    fn first_value_ptr(&self) -> *const u8 {
        panic!("first_value_ptr for Hashmap not implemented");
    }
}
