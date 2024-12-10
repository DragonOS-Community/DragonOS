mod consts;
mod print;

use crate::bpf::helper::print::trace_printf;
use crate::bpf::map::{BpfCallBackFn, BpfMap};
use crate::include::bindings::linux_bpf::BPF_F_CURRENT_CPU;
use crate::libs::lazy_init::Lazy;
use crate::smp::core::smp_get_processor_id;
use crate::time::Instant;
use alloc::{collections::BTreeMap, sync::Arc};
use core::ffi::c_void;
use system_error::SystemError;

type RawBPFHelperFn = fn(u64, u64, u64, u64, u64) -> u64;
type Result<T> = core::result::Result<T, SystemError>;
macro_rules! define_func {
    ($name:ident) => {
        core::mem::transmute::<usize, RawBPFHelperFn>($name as usize)
    };
}

/// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_map_lookup_elem/
unsafe fn raw_map_lookup_elem(map: *mut c_void, key: *const c_void) -> *const c_void {
    let map = Arc::from_raw(map as *const BpfMap);
    let key_size = map.key_size();
    let key = core::slice::from_raw_parts(key as *const u8, key_size);
    let value = map_lookup_elem(&map, key);
    // log::info!("<raw_map_lookup_elem>: {:x?}", value);
    // warning: We need to keep the map alive, so we don't drop it here.
    let _ = Arc::into_raw(map);
    match value {
        Ok(Some(value)) => value as *const c_void,
        _ => core::ptr::null_mut(),
    }
}

pub fn map_lookup_elem(map: &Arc<BpfMap>, key: &[u8]) -> Result<Option<*const u8>> {
    let mut binding = map.inner_map().lock();
    let value = binding.lookup_elem(key);
    match value {
        Ok(Some(value)) => Ok(Some(value.as_ptr())),
        _ => Ok(None),
    }
}

/// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_perf_event_output/
///
/// See https://man7.org/linux/man-pages/man7/bpf-helpers.7.html
unsafe fn raw_perf_event_output(
    ctx: *mut c_void,
    map: *mut c_void,
    flags: u64,
    data: *mut c_void,
    size: u64,
) -> i64 {
    // log::info!("<raw_perf_event_output>: {:x?}", data);
    let map = Arc::from_raw(map as *const BpfMap);
    let data = core::slice::from_raw_parts(data as *const u8, size as usize);
    let res = perf_event_output(ctx, &map, flags, data);
    // warning: We need to keep the map alive, so we don't drop it here.
    let _ = Arc::into_raw(map);
    match res {
        Ok(_) => 0,
        Err(e) => e as i64,
    }
}

pub fn perf_event_output(
    ctx: *mut c_void,
    map: &Arc<BpfMap>,
    flags: u64,
    data: &[u8],
) -> Result<()> {
    let mut binding = map.inner_map().lock();
    let index = flags as u32;
    let flags = (flags >> 32) as u32;
    let key = if index == BPF_F_CURRENT_CPU as u32 {
        smp_get_processor_id().data()
    } else {
        index
    };
    let fd = binding
        .lookup_elem(&key.to_ne_bytes())?
        .ok_or(SystemError::ENOENT)?;
    let fd = u32::from_ne_bytes(fd.try_into().map_err(|_| SystemError::EINVAL)?);
    crate::perf::perf_event_output(ctx, fd as usize, flags, data)?;
    Ok(())
}

/// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_probe_read/
fn raw_bpf_probe_read(dst: *mut c_void, size: u32, unsafe_ptr: *const c_void) -> i64 {
    log::info!(
        "raw_bpf_probe_read, dst:{:x}, size:{}, unsafe_ptr: {:x}",
        dst as usize,
        size,
        unsafe_ptr as usize
    );
    let (dst, src) = unsafe {
        let dst = core::slice::from_raw_parts_mut(dst as *mut u8, size as usize);
        let src = core::slice::from_raw_parts(unsafe_ptr as *const u8, size as usize);
        (dst, src)
    };
    let res = bpf_probe_read(dst, src);
    match res {
        Ok(_) => 0,
        Err(e) => e as i64,
    }
}

/// For tracing programs, safely attempt to read size
/// bytes from kernel space address unsafe_ptr and
/// store the data in dst.
pub fn bpf_probe_read(dst: &mut [u8], src: &[u8]) -> Result<()> {
    log::info!("bpf_probe_read: len: {}", dst.len());
    dst.copy_from_slice(src);
    Ok(())
}

unsafe fn raw_map_update_elem(
    map: *mut c_void,
    key: *const c_void,
    value: *const c_void,
    flags: u64,
) -> i64 {
    let map = Arc::from_raw(map as *const BpfMap);
    let key_size = map.key_size();
    let value_size = map.value_size();
    // log::info!("<raw_map_update_elem>: flags: {:x?}", flags);
    let key = core::slice::from_raw_parts(key as *const u8, key_size);
    let value = core::slice::from_raw_parts(value as *const u8, value_size);
    let res = map_update_elem(&map, key, value, flags);
    let _ = Arc::into_raw(map);
    match res {
        Ok(_) => 0,
        Err(e) => e as _,
    }
}

pub fn map_update_elem(map: &Arc<BpfMap>, key: &[u8], value: &[u8], flags: u64) -> Result<()> {
    let mut binding = map.inner_map().lock();
    let value = binding.update_elem(key, value, flags);
    value
}

/// Delete entry with key from map.
///
/// The delete map element helper call is used to delete values from maps.
unsafe fn raw_map_delete_elem(map: *mut c_void, key: *const c_void) -> i64 {
    let map = Arc::from_raw(map as *const BpfMap);
    let key_size = map.key_size();
    let key = core::slice::from_raw_parts(key as *const u8, key_size);
    let res = map_delete_elem(&map, key);
    let _ = Arc::into_raw(map);
    match res {
        Ok(_) => 0,
        Err(e) => e as i64,
    }
}

pub fn map_delete_elem(map: &Arc<BpfMap>, key: &[u8]) -> Result<()> {
    let mut binding = map.inner_map().lock();
    let value = binding.delete_elem(key);
    value
}

/// For each element in map, call callback_fn function with map, callback_ctx and other map-specific
/// parameters. The callback_fn should be a static function and the callback_ctx should be a pointer
/// to the stack. The flags is used to control certain aspects of the helper.  Currently, the flags must
/// be 0.
///
/// The following are a list of supported map types and their respective expected callback signatures:
/// - BPF_MAP_TYPE_HASH
/// - BPF_MAP_TYPE_PERCPU_HASH
/// - BPF_MAP_TYPE_LRU_HASH
/// - BPF_MAP_TYPE_LRU_PERCPU_HASH
/// - BPF_MAP_TYPE_ARRAY
/// - BPF_MAP_TYPE_PERCPU_ARRAY
///
/// `long (*callback_fn)(struct bpf_map *map, const void key, void *value, void *ctx);`
///
/// For per_cpu maps, the map_value is the value on the cpu where the bpf_prog is running.
unsafe fn raw_map_for_each_elem(
    map: *mut c_void,
    cb: *const c_void,
    ctx: *const c_void,
    flags: u64,
) -> i64 {
    let map = Arc::from_raw(map as *const BpfMap);
    let cb = *core::mem::transmute::<*const c_void, *const BpfCallBackFn>(cb);
    let res = map_for_each_elem(&map, cb, ctx as _, flags);
    let _ = Arc::into_raw(map);
    match res {
        Ok(v) => v as i64,
        Err(e) => e as i64,
    }
}

pub fn map_for_each_elem(
    map: &Arc<BpfMap>,
    cb: BpfCallBackFn,
    ctx: *const u8,
    flags: u64,
) -> Result<u32> {
    let mut binding = map.inner_map().lock();
    let value = binding.for_each_elem(cb, ctx, flags);
    value
}

/// Perform a lookup in percpu map for an entry associated to key on cpu.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_map_lookup_percpu_elem/
unsafe fn raw_map_lookup_percpu_elem(
    map: *mut c_void,
    key: *const c_void,
    cpu: u32,
) -> *const c_void {
    let map = Arc::from_raw(map as *const BpfMap);
    let key_size = map.key_size();
    let key = core::slice::from_raw_parts(key as *const u8, key_size);
    let value = map_lookup_percpu_elem(&map, key, cpu);
    // warning: We need to keep the map alive, so we don't drop it here.
    let _ = Arc::into_raw(map);
    match value {
        Ok(Some(value)) => value as *const c_void,
        _ => core::ptr::null_mut(),
    }
}

pub fn map_lookup_percpu_elem(
    map: &Arc<BpfMap>,
    key: &[u8],
    cpu: u32,
) -> Result<Option<*const u8>> {
    let mut binding = map.inner_map().lock();
    let value = binding.lookup_percpu_elem(key, cpu);
    match value {
        Ok(Some(value)) => Ok(Some(value.as_ptr())),
        _ => Ok(None),
    }
}
/// Push an element value in map.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_map_push_elem/
unsafe fn raw_map_push_elem(map: *mut c_void, value: *const c_void, flags: u64) -> i64 {
    let map = Arc::from_raw(map as *const BpfMap);
    let value_size = map.value_size();
    let value = core::slice::from_raw_parts(value as *const u8, value_size);
    let res = map_push_elem(&map, value, flags);
    let _ = Arc::into_raw(map);
    match res {
        Ok(_) => 0,
        Err(e) => e as i64,
    }
}

pub fn map_push_elem(map: &Arc<BpfMap>, value: &[u8], flags: u64) -> Result<()> {
    let mut binding = map.inner_map().lock();
    let value = binding.push_elem(value, flags);
    value
}

/// Pop an element from map.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_map_pop_elem/
unsafe fn raw_map_pop_elem(map: *mut c_void, value: *mut c_void) -> i64 {
    let map = Arc::from_raw(map as *const BpfMap);
    let value_size = map.value_size();
    let value = core::slice::from_raw_parts_mut(value as *mut u8, value_size);
    let res = map_pop_elem(&map, value);
    let _ = Arc::into_raw(map);
    match res {
        Ok(_) => 0,
        Err(e) => e as i64,
    }
}

pub fn map_pop_elem(map: &Arc<BpfMap>, value: &mut [u8]) -> Result<()> {
    let mut binding = map.inner_map().lock();
    let value = binding.pop_elem(value);
    value
}

/// Get an element from map without removing it.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_map_peek_elem/
unsafe fn raw_map_peek_elem(map: *mut c_void, value: *mut c_void) -> i64 {
    let map = Arc::from_raw(map as *const BpfMap);
    let value_size = map.value_size();
    let value = core::slice::from_raw_parts_mut(value as *mut u8, value_size);
    let res = map_peek_elem(&map, value);
    let _ = Arc::into_raw(map);
    match res {
        Ok(_) => 0,
        Err(e) => e as i64,
    }
}

pub fn map_peek_elem(map: &Arc<BpfMap>, value: &mut [u8]) -> Result<()> {
    let binding = map.inner_map().lock();
    let value = binding.peek_elem(value);
    value
}

pub fn bpf_ktime_get_ns() -> u64 {
    (Instant::now().total_micros() * 1000) as u64
}

pub static BPF_HELPER_FUN_SET: Lazy<BTreeMap<u32, RawBPFHelperFn>> = Lazy::new();

/// Initialize the helper functions.
pub fn init_helper_functions() {
    use consts::*;
    let mut map = BTreeMap::new();
    unsafe {
        // Map helpers::Generic map helpers
        map.insert(HELPER_MAP_LOOKUP_ELEM, define_func!(raw_map_lookup_elem));
        map.insert(HELPER_MAP_UPDATE_ELEM, define_func!(raw_map_update_elem));
        map.insert(HELPER_MAP_DELETE_ELEM, define_func!(raw_map_delete_elem));
        map.insert(HELPER_KTIME_GET_NS, define_func!(bpf_ktime_get_ns));
        map.insert(
            HELPER_MAP_FOR_EACH_ELEM,
            define_func!(raw_map_for_each_elem),
        );
        map.insert(
            HELPER_MAP_LOOKUP_PERCPU_ELEM,
            define_func!(raw_map_lookup_percpu_elem),
        );
        // map.insert(93,define_func!(raw_bpf_spin_lock);
        // map.insert(94,define_func!(raw_bpf_spin_unlock);
        // Map helpers::Perf event array helpers
        map.insert(
            HELPER_PERF_EVENT_OUTPUT,
            define_func!(raw_perf_event_output),
        );
        // Probe and trace helpers::Memory helpers
        map.insert(HELPER_BPF_PROBE_READ, define_func!(raw_bpf_probe_read));
        // Print helpers
        map.insert(HELPER_TRACE_PRINTF, define_func!(trace_printf));

        // Map helpers::Queue and stack helpers
        map.insert(HELPER_MAP_PUSH_ELEM, define_func!(raw_map_push_elem));
        map.insert(HELPER_MAP_POP_ELEM, define_func!(raw_map_pop_elem));
        map.insert(HELPER_MAP_PEEK_ELEM, define_func!(raw_map_peek_elem));
    }
    BPF_HELPER_FUN_SET.init(map);
}
