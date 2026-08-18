use crate::libs::spinlock::SpinLock;
use alloc::{boxed::Box, collections::BTreeMap, format, string::String, sync::Arc, vec::Vec};
use core::{
    any::Any,
    fmt::Debug,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};
use static_keys::StaticFalseKey;

#[derive(Debug)]
#[repr(C, packed)]
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
    callback: SpinLock<Option<Arc<[TracePointFunc]>>>,
    raw_callback: SpinLock<BTreeMap<usize, Arc<dyn TracePointCallBackFunc>>>,
    enable_state: SpinLock<TracePointEnableState>,
    trace_pipe_enabled: AtomicBool,
    trace_entry_fmt_func: fn(&[u8]) -> String,
    trace_print_func: fn() -> String,
    flags: u8,
}

#[derive(Debug)]
struct TracePointEnableState {
    users: usize,
    trace_pipe_enabled: bool,
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

#[derive(Debug, Clone)]
pub struct TracePointFunc {
    pub func: fn(),
    pub data: Arc<dyn Any + Send + Sync>,
}

pub trait TracePointCallBackFunc: Send + Sync {
    fn call(&self, entry: &[u8]);
}

/// Scalar field types supported by dynamically-sized trace records.
///
/// The codec writes fields explicitly instead of copying a Rust structure's
/// object representation, so padding bytes never become part of the ABI.
pub trait TraceEventField: Copy {
    const SIZE: usize;
    const ALIGN: usize;
    const SIGNED: bool;
    const TYPE_NAME: &'static str;

    fn write_ne(self, dst: &mut [u8]) -> bool;
    fn read_ne(src: &[u8]) -> Option<Self>;
}

macro_rules! impl_trace_event_field {
    ($($ty:ty),* $(,)?) => {
        $(
            impl TraceEventField for $ty {
                const SIZE: usize = core::mem::size_of::<Self>();
                const ALIGN: usize = core::mem::align_of::<Self>();
                const SIGNED: bool = <$ty>::MIN != 0;
                const TYPE_NAME: &'static str = stringify!($ty);

                fn write_ne(self, dst: &mut [u8]) -> bool {
                    let bytes = self.to_ne_bytes();
                    if dst.len() != bytes.len() {
                        return false;
                    }
                    dst.copy_from_slice(&bytes);
                    true
                }

                fn read_ne(src: &[u8]) -> Option<Self> {
                    Some(<$ty>::from_ne_bytes(src.try_into().ok()?))
                }
            }
        )*
    };
}

impl_trace_event_field!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

impl TracePoint {
    pub const fn new(
        key: &'static StaticFalseKey,
        name: &'static str,
        system: &'static str,
        fmt_func: fn(&[u8]) -> String,
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
            callback: SpinLock::new(None),
            raw_callback: SpinLock::new(BTreeMap::new()),
            enable_state: SpinLock::new(TracePointEnableState {
                users: 0,
                trace_pipe_enabled: false,
            }),
            trace_pipe_enabled: AtomicBool::new(false),
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
    pub(crate) fn fmt_func(&self) -> fn(&[u8]) -> String {
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
        let ptr = func as usize;
        let mut callbacks = self.callback.lock();
        if callbacks
            .as_deref()
            .is_some_and(|current| current.iter().any(|item| item.func as usize == ptr))
        {
            return;
        }

        let mut updated = callbacks
            .as_deref()
            .map(<[TracePointFunc]>::to_vec)
            .unwrap_or_default();
        updated.push(TracePointFunc {
            func,
            data: data.into(),
        });
        *callbacks = Some(updated.into());
    }

    /// Unregister a callback function from the tracepoint
    pub fn unregister(&self, func: fn()) {
        let func_ptr = func as usize;
        let mut callbacks = self.callback.lock();
        let Some(current) = callbacks.as_deref() else {
            return;
        };
        let updated: Vec<_> = current
            .iter()
            .filter(|item| item.func as usize != func_ptr)
            .cloned()
            .collect();
        if updated.len() == current.len() {
            return;
        }
        *callbacks = if updated.is_empty() {
            None
        } else {
            Some(updated.into())
        };
    }

    /// Iterate over all registered callback functions
    pub fn callback_list(&self, f: &dyn Fn(&TracePointFunc)) {
        let callbacks = self.callback.lock().clone();
        if let Some(callbacks) = callbacks {
            for trace_func in callbacks.iter() {
                f(trace_func);
            }
        }
    }

    /// Register a raw callback function to the tracepoint
    ///
    /// This function will be called when default tracepoint fmt function is called.
    pub fn register_raw_callback(
        &self,
        callback_id: usize,
        callback: Arc<dyn TracePointCallBackFunc>,
    ) {
        self.raw_callback
            .lock()
            .entry(callback_id)
            .or_insert(callback);
    }

    /// Unregister a raw callback function from the tracepoint
    pub fn unregister_raw_callback(&self, callback_id: usize) {
        self.raw_callback.lock().remove(&callback_id);
    }

    /// Snapshot all registered raw callbacks.
    ///
    /// Callback execution may allocate and run arbitrary BPF code, so it must
    /// not happen while the registry spin lock is held. `Arc` keeps callbacks
    /// alive when an owner unregisters after this snapshot was taken.
    pub fn raw_callbacks_snapshot(&self) -> Vec<Arc<dyn TracePointCallBackFunc>> {
        self.raw_callback.lock().values().cloned().collect()
    }

    /// Acquire one active consumer reference.
    pub fn acquire_enable(&self) {
        let mut state = self.enable_state.lock();
        let users = state
            .users
            .checked_add(1)
            .expect("tracepoint enable reference overflow");
        if state.users == 0 {
            unsafe { self.key.enable() };
        }
        state.users = users;
    }

    /// Release one active consumer reference.
    pub fn release_enable(&self) {
        let mut state = self.enable_state.lock();
        state.users = state
            .users
            .checked_sub(1)
            .expect("tracepoint enable reference underflow");
        if state.users == 0 {
            unsafe { self.key.disable() };
        }
    }

    /// Set the tracefs recording owner state. Repeated writes are idempotent.
    pub fn set_trace_pipe_enabled(&self, enabled: bool) {
        let mut state = self.enable_state.lock();
        if state.trace_pipe_enabled == enabled {
            return;
        }

        if enabled {
            let users = state
                .users
                .checked_add(1)
                .expect("tracepoint enable reference overflow");
            if state.users == 0 {
                unsafe { self.key.enable() };
            }
            state.users = users;
            state.trace_pipe_enabled = true;
            self.trace_pipe_enabled.store(true, Ordering::Release);
        } else {
            self.trace_pipe_enabled.store(false, Ordering::Release);
            state.trace_pipe_enabled = false;
            state.users = state
                .users
                .checked_sub(1)
                .expect("tracepoint enable reference underflow");
            if state.users == 0 {
                unsafe { self.key.disable() };
            }
        }
    }

    /// Whether tracefs owns an active recording reference.
    pub fn is_trace_pipe_enabled(&self) -> bool {
        self.trace_pipe_enabled.load(Ordering::Acquire)
    }
}
