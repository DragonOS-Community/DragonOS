#![allow(function_casts_as_integer)]

mod consts;
mod print;

use crate::bpf::helper::print::trace_printf;
use crate::bpf::map::ringbuf::{ringbuf_discard, ringbuf_reserve, ringbuf_submit};
use crate::bpf::map::{BpfCallBackFn, BpfMap};
use crate::include::bindings::linux_bpf::BPF_F_CURRENT_CPU;
use crate::libs::lazy_init::Lazy;
use crate::mm::VirtAddr;
use crate::process::ProcessManager;
use crate::smp::core::smp_get_processor_id;
use crate::syscall::user_access::copy_from_user_protected;
use crate::time::Instant;
use alloc::collections::BTreeMap;
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
    // The program holds the map alive; the helper only borrows it.
    let map = unsafe { &*(map as *const BpfMap) };
    let key = unsafe { core::slice::from_raw_parts(key as *const u8, map.key_size()) };
    match map_lookup_elem(map, key) {
        Ok(Some(value)) => value as *const c_void,
        _ => core::ptr::null_mut(),
    }
}

pub fn map_lookup_elem(map: &BpfMap, key: &[u8]) -> Result<Option<*const u8>> {
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
    let map = unsafe { &*(map as *const BpfMap) };
    let data = unsafe { core::slice::from_raw_parts(data as *const u8, size as usize) };
    match perf_event_output(ctx, map, flags, data) {
        Ok(_) => 0,
        Err(e) => e as i64,
    }
}

pub fn perf_event_output(ctx: *mut c_void, map: &BpfMap, flags: u64, data: &[u8]) -> Result<()> {
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
    // log::info!(
    //     "raw_bpf_probe_read, dst:{:x}, size:{}, unsafe_ptr: {:x}",
    //     dst as usize,
    //     size,
    //     unsafe_ptr as usize
    // );
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
    // log::info!("bpf_probe_read: len: {}", dst.len());
    dst.copy_from_slice(src);
    Ok(())
}

unsafe fn raw_map_update_elem(
    map: *mut c_void,
    key: *const c_void,
    value: *const c_void,
    flags: u64,
) -> i64 {
    let map = unsafe { &*(map as *const BpfMap) };
    let key_size = map.key_size();
    let value_size = map.value_size();
    // log::info!("<raw_map_update_elem>: flags: {:x?}", flags);
    let key = unsafe { core::slice::from_raw_parts(key as *const u8, key_size) };
    let value = unsafe { core::slice::from_raw_parts(value as *const u8, value_size) };
    match map_update_elem(map, key, value, flags) {
        Ok(_) => 0,
        Err(e) => e as _,
    }
}

pub fn map_update_elem(map: &BpfMap, key: &[u8], value: &[u8], flags: u64) -> Result<()> {
    let mut binding = map.inner_map().lock();
    let value = binding.update_elem(key, value, flags);
    value
}

/// Delete entry with key from map.
///
/// The delete map element helper call is used to delete values from maps.
unsafe fn raw_map_delete_elem(map: *mut c_void, key: *const c_void) -> i64 {
    let map = unsafe { &*(map as *const BpfMap) };
    let key = unsafe { core::slice::from_raw_parts(key as *const u8, map.key_size()) };
    match map_delete_elem(map, key) {
        Ok(_) => 0,
        Err(e) => e as i64,
    }
}

pub fn map_delete_elem(map: &BpfMap, key: &[u8]) -> Result<()> {
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
    let map = unsafe { &*(map as *const BpfMap) };
    let cb = unsafe { *core::mem::transmute::<*const c_void, *const BpfCallBackFn>(cb) };
    match map_for_each_elem(map, cb, ctx as _, flags) {
        Ok(v) => v as i64,
        Err(e) => e as i64,
    }
}

pub fn map_for_each_elem(
    map: &BpfMap,
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
    let map = unsafe { &*(map as *const BpfMap) };
    let key = unsafe { core::slice::from_raw_parts(key as *const u8, map.key_size()) };
    match map_lookup_percpu_elem(map, key, cpu) {
        Ok(Some(value)) => value as *const c_void,
        _ => core::ptr::null_mut(),
    }
}

pub fn map_lookup_percpu_elem(map: &BpfMap, key: &[u8], cpu: u32) -> Result<Option<*const u8>> {
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
    let map = unsafe { &*(map as *const BpfMap) };
    let value = unsafe { core::slice::from_raw_parts(value as *const u8, map.value_size()) };
    match map_push_elem(map, value, flags) {
        Ok(_) => 0,
        Err(e) => e as i64,
    }
}

pub fn map_push_elem(map: &BpfMap, value: &[u8], flags: u64) -> Result<()> {
    let mut binding = map.inner_map().lock();
    let value = binding.push_elem(value, flags);
    value
}

/// Pop an element from map.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_map_pop_elem/
unsafe fn raw_map_pop_elem(map: *mut c_void, value: *mut c_void) -> i64 {
    let map = unsafe { &*(map as *const BpfMap) };
    let value = unsafe { core::slice::from_raw_parts_mut(value as *mut u8, map.value_size()) };
    match map_pop_elem(map, value) {
        Ok(_) => 0,
        Err(e) => e as i64,
    }
}

pub fn map_pop_elem(map: &BpfMap, value: &mut [u8]) -> Result<()> {
    let mut binding = map.inner_map().lock();
    let value = binding.pop_elem(value);
    value
}

/// Get an element from map without removing it.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_map_peek_elem/
unsafe fn raw_map_peek_elem(map: *mut c_void, value: *mut c_void) -> i64 {
    let map = unsafe { &*(map as *const BpfMap) };
    let value = unsafe { core::slice::from_raw_parts_mut(value as *mut u8, map.value_size()) };
    match map_peek_elem(map, value) {
        Ok(_) => 0,
        Err(e) => e as i64,
    }
}

pub fn map_peek_elem(map: &BpfMap, value: &mut [u8]) -> Result<()> {
    let binding = map.inner_map().lock();
    let value = binding.peek_elem(value);
    value
}

pub fn bpf_ktime_get_ns() -> u64 {
    (Instant::now().total_micros() * 1000) as u64
}

/// Copy a NULL terminated string from an unsafe user address unsafe_ptr to dst.
/// The size should include the terminating NULL byte. In case the string length is smaller than size,
/// the target is not padded with further NULL bytes. If the string length is larger than size,
/// just size-1 bytes are copied and the last byte is set to NULL.
///
/// On success, the strictly positive length of the output string, including the trailing NULL character. On error, a negative value
///
/// See https://docs.ebpf.io/linux/helper-function/bpf_probe_read_user_str/
unsafe fn raw_probe_read_user_str(dst: *mut c_void, size: u32, unsafe_ptr: *const c_void) -> i64 {
    // log::info!(
    //     "<raw_probe_read_user_str>: dst:{:x}, size:{}, unsafe_ptr: {:x}",
    //     dst as usize,
    //     size,
    //     unsafe_ptr as usize
    // );
    let dst = core::slice::from_raw_parts_mut(dst as *mut u8, size as usize);
    let res = probe_read_user_str(dst, unsafe_ptr as *const u8);
    match res {
        Ok(len) => len as i64,
        Err(e) => e as i64,
    }
}

pub fn probe_read_user_str(dst: &mut [u8], src: *const u8) -> Result<usize> {
    // A BPF program may pass any pointer. Match Linux strncpy_from_user
    // semantics: stop at the first NUL (or at size-1), and stop cleanly at
    // the first unreadable byte instead of faulting. A whole-buffer copy
    // would fault past short strings that end near a page boundary and
    // discard the readable prefix.
    if dst.is_empty() {
        return Err(SystemError::EFAULT);
    }
    let mut end = 0usize;
    let limit = dst.len() - 1;
    while end < limit {
        let mut byte = [0u8; 1];
        let ok = unsafe { copy_from_user_protected(&mut byte, VirtAddr::new(src as usize + end)) }
            .is_ok();
        if !ok || byte[0] == 0 {
            break;
        }
        dst[end] = byte[0];
        end += 1;
    }
    dst[end] = 0;
    Ok(end + 1)
}

/// A helper function that returns the process ID of the current process
/// (upper 32 bits, the thread group id) and the thread ID of the current
/// thread (lower 32 bits).
///
/// See https://docs.ebpf.io/linux/helper-function/bpf_get_current_pid_tgid/
unsafe fn raw_get_current_pid_tgid() -> u64 {
    let pcb = ProcessManager::current_pcb();
    ((pcb.raw_tgid().data() as u64) << 32) | (pcb.raw_pid().data() as u64)
}

/// A helper function that returns the real UID (upper 32 bits) and the real
/// GID (lower 32 bits) of the current task.
///
/// See https://docs.ebpf.io/linux/helper-function/bpf_get_current_uid_gid/
unsafe fn raw_get_current_uid_gid() -> u64 {
    let cred = ProcessManager::current_pcb().cred();
    ((cred.uid.data() as u64) << 32) | (cred.gid.data() as u64)
}

/// Copy the comm attribute of the current task into buf (of size min(size,
/// TASK_COMM_LEN)), NUL-terminated.
///
/// See https://docs.ebpf.io/linux/helper-function/bpf_get_current_comm/
unsafe fn raw_get_current_comm(buf: *mut c_void, size: u64) -> i64 {
    if size == 0 {
        return 0;
    }
    let pcb = ProcessManager::current_pcb();
    let dst = core::slice::from_raw_parts_mut(buf as *mut u8, size as usize);
    let basic = pcb.basic();
    let name = basic.name().as_bytes();
    // Linux comm is the basename of argv[0], truncated to TASK_COMM_LEN-1;
    // DragonOS stores the full argv[0] here.
    let basename = match name.iter().rposition(|b| *b == b'/') {
        Some(pos) => &name[pos + 1..],
        None => name,
    };
    let copy_len = basename.len().min((size as usize - 1).min(15));
    dst[..copy_len].copy_from_slice(&basename[..copy_len]);
    dst[copy_len] = 0;
    0
}

/// store the bytes in dst. Unlike `bpf_probe_read`, faults on the user address
/// are caught (exception-table protected copy) and reported as -EFAULT instead
/// of crashing the kernel.
///
/// See https://docs.ebpf.io/linux/helper-function/bpf_probe_read_user/
unsafe fn raw_probe_read_user(dst: *mut c_void, size: u32, unsafe_ptr: *const c_void) -> i64 {
    if size == 0 {
        return 0;
    }
    let dst = core::slice::from_raw_parts_mut(dst as *mut u8, size as usize);
    let res = copy_from_user_protected(dst, VirtAddr::new(unsafe_ptr as usize));
    match res {
        Ok(_) => 0,
        Err(e) => e as i64,
    }
}

/// See https://docs.ebpf.io/linux/helper-function/bpf_ringbuf_reserve/
unsafe fn raw_ringbuf_reserve(map: *mut c_void, size: u64, flags: u64) -> u64 {
    let map = unsafe { &*(map as *const BpfMap) };
    ringbuf_reserve(map, size, flags) as u64
}

/// See https://docs.ebpf.io/linux/helper-function/bpf_ringbuf_submit/
unsafe fn raw_ringbuf_submit(data: *mut c_void, flags: u64) -> u64 {
    ringbuf_submit(data as *mut u8, flags);
    0
}

/// See https://docs.ebpf.io/linux/helper-function/bpf_ringbuf_discard/
unsafe fn raw_ringbuf_discard(data: *mut c_void, flags: u64) -> u64 {
    ringbuf_discard(data as *mut u8, flags);
    0
}

/// See https://docs.ebpf.io/linux/helper-function/bpf_ringbuf_output/
unsafe fn raw_ringbuf_output(map: *mut c_void, data: *mut c_void, size: u64, _flags: u64) -> u64 {
    let map = unsafe { &*(map as *const BpfMap) };
    let dst = ringbuf_reserve(map, size, 0);
    if dst.is_null() {
        return u64::MAX; // -EINVAL
    }
    let src = unsafe { core::slice::from_raw_parts(data as *const u8, size as usize) };
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
    }
    ringbuf_submit(dst, 0);
    0
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

        // User access helpers
        map.insert(HELPER_PROBE_READ_USER, define_func!(raw_probe_read_user));
        map.insert(
            HELPER_PROBE_READ_USER_STR,
            define_func!(raw_probe_read_user_str),
        );

        // Current task helpers
        map.insert(
            HELPER_GET_CURRENT_PID_TGID,
            define_func!(raw_get_current_pid_tgid),
        );
        map.insert(
            HELPER_GET_CURRENT_UID_GID,
            define_func!(raw_get_current_uid_gid),
        );
        map.insert(HELPER_GET_CURRENT_COMM, define_func!(raw_get_current_comm));

        // Ring buffer helpers
        map.insert(HELPER_RINGBUF_OUTPUT, define_func!(raw_ringbuf_output));
        map.insert(HELPER_RINGBUF_RESERVE, define_func!(raw_ringbuf_reserve));
        map.insert(HELPER_RINGBUF_SUBMIT, define_func!(raw_ringbuf_submit));
        map.insert(HELPER_RINGBUF_DISCARD, define_func!(raw_ringbuf_discard));
    }
    BPF_HELPER_FUN_SET.init(map);
}
