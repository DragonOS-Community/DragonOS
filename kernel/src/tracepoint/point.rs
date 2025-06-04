use crate::libs::spinlock::SpinLock;
use alloc::{boxed::Box, collections::BTreeMap, format, string::String};
use core::{any::Any, sync::atomic::AtomicU32};
use static_keys::StaticFalseKey;

#[derive(Debug)]
#[repr(C)]
pub struct TraceEntry {
    pub type_: u16,
    pub flags: u8,
    pub preempt_count: u8,
    pub pid: i32,
}

impl TraceEntry {
    pub fn trace_print_lat_fmt(&self) -> String {
        // todo!("Implement IRQs off logic");
        let irqs_off = '.';
        let resched = '.';
        let hardsoft_irq = '.';
        let mut preempt_low = '.';
        if self.preempt_count & 0xf != 0 {
            preempt_low = ((b'0') + (self.preempt_count & 0xf)) as char;
        }
        let mut preempt_high = '.';
        if self.preempt_count >> 4 != 0 {
            preempt_high = ((b'0') + (self.preempt_count >> 4)) as char;
        }
        format!(
            "{}{}{}{}{}",
            irqs_off, resched, hardsoft_irq, preempt_low, preempt_high
        )
    }
}

pub struct TracePoint {
    name: &'static str,
    system: &'static str,
    key: &'static StaticFalseKey,
    id: AtomicU32,
    inner: SpinLock<TracePointInner>,
    trace_entry_fmt_func: fn(*const u8) -> String,
    trace_print_func: fn() -> String,
    flags: u8,
}

struct TracePointInner {
    callback: BTreeMap<usize, TracePointFunc>,
}

impl core::fmt::Debug for TracePoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TracePoint")
            .field("name", &self.name)
            .field("system", &self.system)
            .field("id", &self.id())
            .field("flags", &self.flags)
            .finish()
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct CommonTracePointMeta {
    pub trace_point: &'static TracePoint,
    pub print_func: fn(),
}

#[derive(Debug)]
pub struct TracePointFunc {
    pub func: fn(),
    pub data: Box<dyn Any + Send + Sync>,
}

impl TracePoint {
    pub const fn new(
        key: &'static StaticFalseKey,
        name: &'static str,
        system: &'static str,
        fmt_func: fn(*const u8) -> String,
        trace_print_func: fn() -> String,
    ) -> Self {
        Self {
            name,
            system,
            key,
            id: AtomicU32::new(0),
            flags: 0,
            trace_entry_fmt_func: fmt_func,
            trace_print_func,
            inner: SpinLock::new(TracePointInner {
                callback: BTreeMap::new(),
            }),
        }
    }

    /// Returns the name of the tracepoint.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Returns the system of the tracepoint.
    pub fn system(&self) -> &'static str {
        self.system
    }

    /// Sets the ID of the tracepoint.
    pub(crate) fn set_id(&self, id: u32) {
        self.id.store(id, core::sync::atomic::Ordering::Relaxed);
    }

    /// Returns the ID of the tracepoint.
    pub fn id(&self) -> u32 {
        self.id.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Returns the flags of the tracepoint.
    pub fn flags(&self) -> u8 {
        self.flags
    }

    /// Returns the format function for the tracepoint.
    pub(crate) fn fmt_func(&self) -> fn(*const u8) -> String {
        self.trace_entry_fmt_func
    }

    /// Returns a string representation of the format function for the tracepoint.
    ///
    /// You can use `cat /sys/kernel/debug/tracing/events/syscalls/sys_enter_openat/format` in linux
    /// to see the format of the tracepoint.
    pub fn print_fmt(&self) -> String {
        let post_str = (self.trace_print_func)();
        format!("name: {}\nID: {}\n{}\n", self.name(), self.id(), post_str)
    }

    /// Register a callback function to the tracepoint
    pub fn register(&self, func: fn(), data: Box<dyn Any + Sync + Send>) {
        let trace_point_func = TracePointFunc { func, data };
        let ptr = func as usize;
        self.inner
            .lock()
            .callback
            .entry(ptr)
            .or_insert(trace_point_func);
    }

    /// Unregister a callback function from the tracepoint
    pub fn unregister(&self, func: fn()) {
        let func_ptr = func as usize;
        self.inner.lock().callback.remove(&func_ptr);
    }

    /// Iterate over all registered callback functions
    pub fn callback_list(&self, f: &dyn Fn(&TracePointFunc)) {
        let inner = self.inner.lock();
        for trace_func in inner.callback.values() {
            f(trace_func);
        }
    }

    /// Enable the tracepoint
    pub fn enable(&self) {
        unsafe {
            self.key.enable();
        }
    }

    /// Disable the tracepoint
    pub fn disable(&self) {
        unsafe {
            self.key.disable();
        }
    }

    /// Check if the tracepoint is enabled
    pub fn is_enabled(&self) -> bool {
        self.key.is_enabled()
    }
}
