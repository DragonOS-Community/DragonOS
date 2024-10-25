mod array_map;
mod hash_map;
mod lru;
mod queue;
mod util;

use super::Result;
use crate::bpf::map::array_map::{ArrayMap, PerCpuArrayMap, PerfEventArrayMap};
use crate::bpf::map::hash_map::PerCpuHashMap;
use crate::bpf::map::util::{BpfMapGetNextKeyArg, BpfMapMeta, BpfMapUpdateArg};
use crate::filesystem::vfs::file::{File, FileMode};
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::{FilePrivateData, FileSystem, FileType, IndexNode, Metadata};
use crate::include::bindings::linux_bpf::{bpf_attr, bpf_map_type};
use crate::libs::casting::DowncastArc;
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::process::ProcessManager;
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::Debug;
use intertrait::CastFromSync;
use log::{error, info};
use system_error::SystemError;

#[derive(Debug)]
pub struct BpfMap {
    inner_map: SpinLock<Box<dyn BpfMapCommonOps>>,
    meta: BpfMapMeta,
}

pub type BpfCallBackFn = fn(key: &[u8], value: &[u8], ctx: *const u8) -> i32;

pub trait BpfMapCommonOps: Send + Sync + Debug + CastFromSync {
    /// Lookup an element in the map.
    ///
    /// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_map_lookup_elem/
    fn lookup_elem(&mut self, _key: &[u8]) -> Result<Option<&[u8]>> {
        Err(SystemError::ENOSYS)
    }
    /// Update an element in the map.
    ///
    /// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_map_update_elem/
    fn update_elem(&mut self, _key: &[u8], _value: &[u8], _flags: u64) -> Result<()> {
        Err(SystemError::ENOSYS)
    }
    /// Delete an element from the map.
    ///
    /// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_map_delete_elem/
    fn delete_elem(&mut self, _key: &[u8]) -> Result<()> {
        Err(SystemError::ENOSYS)
    }
    /// For each element in map, call callback_fn function with map,
    /// callback_ctx and other map-specific parameters.
    ///
    /// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_for_each_map_elem/
    fn for_each_elem(&mut self, _cb: BpfCallBackFn, _ctx: *const u8, _flags: u64) -> Result<u32> {
        Err(SystemError::ENOSYS)
    }
    /// Look up an element with the given key in the map referred to by the file descriptor fd,
    /// and if found, delete the element.
    fn lookup_and_delete_elem(&mut self, _key: &[u8], _value: &mut [u8]) -> Result<()> {
        Err(SystemError::ENOSYS)
    }

    /// erform a lookup in percpu map for an entry associated to key on cpu.
    fn lookup_percpu_elem(&mut self, _key: &[u8], _cpu: u32) -> Result<Option<&[u8]>> {
        Err(SystemError::ENOSYS)
    }
    /// Get the next key in the map. If key is None, get the first key.
    ///
    /// Called from syscall
    fn get_next_key(&self, _key: Option<&[u8]>, _next_key: &mut [u8]) -> Result<()> {
        Err(SystemError::ENOSYS)
    }

    /// Push an element value in map.
    fn push_elem(&mut self, _value: &[u8], _flags: u64) -> Result<()> {
        Err(SystemError::ENOSYS)
    }

    /// Pop an element value from map.
    fn pop_elem(&mut self, _value: &mut [u8]) -> Result<()> {
        Err(SystemError::ENOSYS)
    }

    /// Peek an element value from map.
    fn peek_elem(&self, _value: &mut [u8]) -> Result<()> {
        Err(SystemError::ENOSYS)
    }

    /// Freeze the map.
    ///
    /// It's useful for .rodata maps.
    fn freeze(&self) -> Result<()> {
        Err(SystemError::ENOSYS)
    }

    /// Get the first value pointer.
    fn first_value_ptr(&self) -> Result<*const u8> {
        Err(SystemError::ENOSYS)
    }
}
impl DowncastArc for dyn BpfMapCommonOps {
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }
}
impl BpfMap {
    pub fn new(map: Box<dyn BpfMapCommonOps>, meta: BpfMapMeta) -> Self {
        assert_ne!(meta.key_size, 0);
        BpfMap {
            inner_map: SpinLock::new(map),
            meta,
        }
    }

    pub fn inner_map(&self) -> &SpinLock<Box<dyn BpfMapCommonOps>> {
        &self.inner_map
    }

    pub fn key_size(&self) -> usize {
        self.meta.key_size as usize
    }

    pub fn value_size(&self) -> usize {
        self.meta.value_size as usize
    }
}

impl IndexNode for BpfMap {
    fn open(&self, _data: SpinLockGuard<FilePrivateData>, _mode: &FileMode) -> Result<()> {
        Ok(())
    }
    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<()> {
        Ok(())
    }
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize> {
        Err(SystemError::ENOSYS)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize> {
        Err(SystemError::ENOSYS)
    }

    fn metadata(&self) -> Result<Metadata> {
        let meta = Metadata {
            mode: ModeType::from_bits_truncate(0o755),
            file_type: FileType::File,
            ..Default::default()
        };
        Ok(meta)
    }

    fn resize(&self, _len: usize) -> Result<()> {
        Ok(())
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        todo!("BpfMap does not have a filesystem")
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>> {
        Err(SystemError::ENOSYS)
    }
}

/// Create a map and return a file descriptor that refers to
/// the map.  The close-on-exec file descriptor flag
/// is automatically enabled for the new file descriptor.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/syscall/BPF_MAP_CREATE/
pub fn bpf_map_create(attr: &bpf_attr) -> Result<usize> {
    let map_meta = BpfMapMeta::try_from(attr)?;
    info!("The map attr is {:#?}", map_meta);
    let map: Box<dyn BpfMapCommonOps> = match map_meta.map_type {
        bpf_map_type::BPF_MAP_TYPE_ARRAY => {
            let array_map = ArrayMap::new(&map_meta)?;
            Box::new(array_map)
        }
        bpf_map_type::BPF_MAP_TYPE_PERCPU_ARRAY => {
            let per_cpu_array_map = PerCpuArrayMap::new(&map_meta)?;
            Box::new(per_cpu_array_map)
        }
        bpf_map_type::BPF_MAP_TYPE_PERF_EVENT_ARRAY => {
            let perf_event_array_map = PerfEventArrayMap::new(&map_meta)?;
            Box::new(perf_event_array_map)
        }

        bpf_map_type::BPF_MAP_TYPE_CPUMAP
        | bpf_map_type::BPF_MAP_TYPE_DEVMAP
        | bpf_map_type::BPF_MAP_TYPE_DEVMAP_HASH => {
            error!("bpf map type {:?} not implemented", map_meta.map_type);
            Err(SystemError::EINVAL)?
        }
        bpf_map_type::BPF_MAP_TYPE_HASH => {
            let hash_map = hash_map::BpfHashMap::new(&map_meta)?;
            Box::new(hash_map)
        }
        bpf_map_type::BPF_MAP_TYPE_PERCPU_HASH => {
            let per_cpu_hash_map = PerCpuHashMap::new(&map_meta)?;
            Box::new(per_cpu_hash_map)
        }
        bpf_map_type::BPF_MAP_TYPE_QUEUE => {
            let queue_map = queue::QueueMap::new(&map_meta)?;
            Box::new(queue_map)
        }
        bpf_map_type::BPF_MAP_TYPE_STACK => {
            let stack_map = queue::StackMap::new(&map_meta)?;
            Box::new(stack_map)
        }
        bpf_map_type::BPF_MAP_TYPE_LRU_HASH => {
            let lru_hash_map = lru::LruMap::new(&map_meta)?;
            Box::new(lru_hash_map)
        }
        bpf_map_type::BPF_MAP_TYPE_LRU_PERCPU_HASH => {
            let lru_per_cpu_hash_map = lru::PerCpuLruMap::new(&map_meta)?;
            Box::new(lru_per_cpu_hash_map)
        }
        _ => {
            unimplemented!("bpf map type {:?} not implemented", map_meta.map_type)
        }
    };
    let bpf_map = BpfMap::new(map, map_meta);
    let fd_table = ProcessManager::current_pcb().fd_table();
    let file = File::new(Arc::new(bpf_map), FileMode::O_RDWR | FileMode::O_CLOEXEC)?;
    let fd = fd_table.write().alloc_fd(file, None).map(|x| x as usize)?;
    info!("create map with fd: [{}]", fd);
    Ok(fd)
}

/// Create or update an element (key/value pair) in a specified map.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/syscall/BPF_MAP_UPDATE_ELEM/
pub fn bpf_map_update_elem(attr: &bpf_attr) -> Result<usize> {
    let arg = BpfMapUpdateArg::from(attr);
    info!("<bpf_map_update_elem>: {:#x?}", arg);
    let map = get_map_file(arg.map_fd as i32)?;
    let meta = &map.meta;
    let key_size = meta.key_size as usize;
    let value_size = meta.value_size as usize;

    let key_buf = UserBufferReader::new(arg.key as *const u8, key_size, true)?;
    let value_buf = UserBufferReader::new(arg.value as *const u8, value_size, true)?;

    let key = key_buf.read_from_user(0)?;
    let value = value_buf.read_from_user(0)?;
    map.inner_map.lock().update_elem(key, value, arg.flags)?;
    info!("bpf_map_update_elem ok");
    Ok(0)
}

pub fn bpf_map_freeze(attr: &bpf_attr) -> Result<usize> {
    let arg = BpfMapUpdateArg::from(attr);
    let map_fd = arg.map_fd;
    info!("<bpf_map_freeze>: map_fd: {:}", map_fd);
    let map = get_map_file(map_fd as i32)?;
    map.inner_map.lock().freeze()?;
    Ok(0)
}

///  Look up an element by key in a specified map and return its value.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/syscall/BPF_MAP_LOOKUP_ELEM/
pub fn bpf_lookup_elem(attr: &bpf_attr) -> Result<usize> {
    let arg = BpfMapUpdateArg::from(attr);
    // info!("<bpf_lookup_elem>: {:#x?}", arg);
    let map = get_map_file(arg.map_fd as _)?;
    let meta = &map.meta;
    let key_size = meta.key_size as usize;
    let value_size = meta.value_size as usize;

    let key_buf = UserBufferReader::new(arg.key as *const u8, key_size, true)?;
    let mut value_buf = UserBufferWriter::new(arg.value as *mut u8, value_size, true)?;

    let key = key_buf.read_from_user(0)?;

    let mut inner = map.inner_map.lock();
    let r_value = inner.lookup_elem(key)?;
    if let Some(r_value) = r_value {
        value_buf.copy_to_user(r_value, 0)?;
        Ok(0)
    } else {
        Err(SystemError::ENOENT)
    }
}
/// Look up an element by key in a specified map and return the key of the next element.
///
/// - If key is `None`, the operation returns zero and sets the next_key pointer to the key of the first element.
/// - If key is `Some(T)`, the operation returns zero and sets the next_key pointer to the key of the next element.
/// - If key is the last element, returns -1 and errno is set to ENOENT.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/syscall/BPF_MAP_GET_NEXT_KEY/
pub fn bpf_map_get_next_key(attr: &bpf_attr) -> Result<usize> {
    let arg = BpfMapGetNextKeyArg::from(attr);
    // info!("<bpf_map_get_next_key>: {:#x?}", arg);
    let map = get_map_file(arg.map_fd as i32)?;
    let meta = &map.meta;
    let key_size = meta.key_size as usize;

    let key = if let Some(key_ptr) = arg.key {
        let key_buf = UserBufferReader::new(key_ptr as *const u8, key_size, true)?;
        let key = key_buf.read_from_user(0)?.to_vec();
        Some(key)
    } else {
        None
    };
    let key = key.as_deref();
    let mut next_key_buf = UserBufferWriter::new(arg.next_key as *mut u8, key_size, true)?;
    let inner = map.inner_map.lock();
    let next_key = next_key_buf.buffer(0)?;
    inner.get_next_key(key, next_key)?;
    // info!("next_key: {:?}", next_key);
    Ok(0)
}

/// Look up and delete an element by key in a specified map.
///
/// # WARN
///
/// Not all map types (particularly array maps) support this operation,
/// instead a zero value can be written to the map value. Check the map types page to check for support.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/syscall/BPF_MAP_DELETE_ELEM/
pub fn bpf_map_delete_elem(attr: &bpf_attr) -> Result<usize> {
    let arg = BpfMapUpdateArg::from(attr);
    // info!("<bpf_map_delete_elem>: {:#x?}", arg);
    let map = get_map_file(arg.map_fd as i32)?;
    let meta = &map.meta;
    let key_size = meta.key_size as usize;

    let key_buf = UserBufferReader::new(arg.key as *const u8, key_size, true)?;
    let key = key_buf.read_from_user(0)?;
    map.inner_map.lock().delete_elem(key)?;
    Ok(0)
}

/// Iterate and fetch multiple elements in a map.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/syscall/BPF_MAP_LOOKUP_BATCH/
pub fn bpf_map_lookup_batch(_attr: &bpf_attr) -> Result<usize> {
    todo!()
}

/// Look up an element with the given key in the map referred to by the file descriptor fd,
/// and if found, delete the element.
///
/// For BPF_MAP_TYPE_QUEUE and BPF_MAP_TYPE_STACK map types, the flags argument needs to be set to 0,
/// but for other map types, it may be specified as:
/// - BPF_F_LOCK : If this flag is set, the command will acquire the spin-lock of the map value we are looking up.
///
/// If the map contains no spin-lock in its value, -EINVAL will be returned by the command.
///
/// The BPF_MAP_TYPE_QUEUE and BPF_MAP_TYPE_STACK map types implement this command as a “pop” operation,
/// deleting the top element rather than one corresponding to key.
/// The key and key_len parameters should be zeroed when issuing this operation for these map types.
///
/// This command is only valid for the following map types:
/// - BPF_MAP_TYPE_QUEUE
/// - BPF_MAP_TYPE_STACK
/// - BPF_MAP_TYPE_HASH
/// - BPF_MAP_TYPE_PERCPU_HASH
/// - BPF_MAP_TYPE_LRU_HASH
/// - BPF_MAP_TYPE_LRU_PERCPU_HASH
///
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/syscall/BPF_MAP_LOOKUP_AND_DELETE_ELEM/
pub fn bpf_map_lookup_and_delete_elem(attr: &bpf_attr) -> Result<usize> {
    let arg = BpfMapUpdateArg::from(attr);
    // info!("<bpf_map_lookup_and_delete_elem>: {:#x?}", arg);
    let map = get_map_file(arg.map_fd as i32)?;
    let meta = &map.meta;
    let key_size = meta.key_size as usize;
    let value_size = meta.value_size as usize;

    let key_buf = UserBufferReader::new(arg.key as *const u8, key_size, true)?;
    let mut value_buf = UserBufferWriter::new(arg.value as *mut u8, value_size, true)?;

    let value = value_buf.buffer(0)?;
    let key = key_buf.read_from_user(0)?;
    let mut inner = map.inner_map.lock();
    inner.lookup_and_delete_elem(key, value)?;
    Ok(0)
}

fn get_map_file(fd: i32) -> Result<Arc<BpfMap>> {
    let fd_table = ProcessManager::current_pcb().fd_table();
    let map = fd_table
        .read()
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    let map = map
        .inode()
        .downcast_arc::<BpfMap>()
        .ok_or(SystemError::EINVAL)?;
    Ok(map)
}
