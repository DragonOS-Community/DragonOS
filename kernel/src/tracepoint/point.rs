use crate::{
    debug::jump_label::{disable_maskable_key, enable_maskable_key, MaskableStaticFalseKey},
    libs::mutex::Mutex,
    process::ProcessManager,
    rcu::srcu::SrcuDomain,
};
use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};
use core::{
    any::Any,
    fmt::Debug,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering},
};
use system_error::SystemError;

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
    key: &'static MaskableStaticFalseKey,
    id: AtomicU32,
    callbacks: AtomicPtr<TracePointSnapshot>,
    callback_update: Mutex<()>,
    enable_state: Mutex<TracePointEnableState>,
    trace_pipe_enabled: AtomicBool,
    trace_entry_fmt_func: fn(&[u8]) -> String,
    trace_print_func: fn() -> String,
    flags: u8,
}

struct TracePointSnapshot {
    callbacks: Vec<TracePointFunc>,
    raw_callbacks: Vec<(usize, Arc<dyn TracePointCallBackFunc>)>,
}

impl TracePointSnapshot {
    fn new() -> Self {
        Self {
            callbacks: Vec::new(),
            raw_callbacks: Vec::new(),
        }
    }

    fn try_clone(&self) -> Result<Self, SystemError> {
        let mut callbacks = Vec::new();
        callbacks
            .try_reserve_exact(self.callbacks.len())
            .map_err(|_| SystemError::ENOMEM)?;
        callbacks.extend(self.callbacks.iter().cloned());
        let mut raw_callbacks = Vec::new();
        raw_callbacks
            .try_reserve_exact(self.raw_callbacks.len())
            .map_err(|_| SystemError::ENOMEM)?;
        raw_callbacks.extend(self.raw_callbacks.iter().cloned());
        Ok(Self {
            callbacks,
            raw_callbacks,
        })
    }
}

static TRACEPOINT_SRCU: AtomicPtr<SrcuDomain> = AtomicPtr::new(ptr::null_mut());

fn tracepoint_srcu() -> Result<&'static SrcuDomain, SystemError> {
    let ptr = TRACEPOINT_SRCU.load(Ordering::Acquire);
    if ptr.is_null() {
        Err(SystemError::ENODEV)
    } else {
        // SAFETY: the global tracepoint domain is intentionally never destroyed.
        Ok(unsafe { &*ptr })
    }
}

pub(crate) fn tracepoint_srcu_init() -> Result<(), SystemError> {
    if !TRACEPOINT_SRCU.load(Ordering::Acquire).is_null() {
        return Ok(());
    }
    let domain =
        Box::try_new(SrcuDomain::try_new("tracepoint")?).map_err(|_| SystemError::ENOMEM)?;
    let raw = Box::into_raw(domain);
    if TRACEPOINT_SRCU
        .compare_exchange(ptr::null_mut(), raw, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // SAFETY: another initializer won publication; this Box was never shared.
        unsafe { drop(Box::from_raw(raw)) };
    }
    Ok(())
}

struct TracepointDepthGuard;

impl TracepointDepthGuard {
    fn enter() -> Self {
        ProcessManager::current_pcb()
            .tracepoint_srcu_depth
            .fetch_add(1, Ordering::Relaxed);
        Self
    }
}

fn tracepoint_read_held_by_current() -> bool {
    ProcessManager::current_pcb()
        .tracepoint_srcu_depth
        .load(Ordering::Relaxed)
        != 0
}

impl Drop for TracepointDepthGuard {
    fn drop(&mut self) {
        let previous = ProcessManager::current_pcb()
            .tracepoint_srcu_depth
            .fetch_sub(1, Ordering::Relaxed);
        assert!(previous != 0, "tracepoint SRCU depth underflow");
    }
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
    pub(crate) const fn new(
        key: &'static MaskableStaticFalseKey,
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
            callbacks: AtomicPtr::new(ptr::null_mut()),
            callback_update: Mutex::new(()),
            enable_state: Mutex::new(TracePointEnableState {
                users: 0,
                trace_pipe_enabled: false,
            }),
            trace_pipe_enabled: AtomicBool::new(false),
        }
    }

    pub(crate) fn init_callbacks(
        &self,
        func: fn(),
        data: Box<dyn Any + Sync + Send>,
    ) -> Result<(), SystemError> {
        if !self.callbacks.load(Ordering::Acquire).is_null() {
            return Ok(());
        }
        let mut snapshot = TracePointSnapshot::new();
        snapshot
            .callbacks
            .try_reserve_exact(1)
            .map_err(|_| SystemError::ENOMEM)?;
        snapshot.callbacks.push(TracePointFunc {
            func,
            data: data.into(),
        });
        let snapshot = Box::try_new(snapshot).map_err(|_| SystemError::ENOMEM)?;
        let raw = Box::into_raw(snapshot);
        if self
            .callbacks
            .compare_exchange(ptr::null_mut(), raw, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // SAFETY: this allocation was not published.
            unsafe { drop(Box::from_raw(raw)) };
        }
        Ok(())
    }

    fn update_callbacks(
        &self,
        change: impl FnOnce(&mut TracePointSnapshot) -> Result<(), SystemError>,
    ) -> Result<(), SystemError> {
        if tracepoint_read_held_by_current() {
            return Err(SystemError::EDEADLK_OR_EDEADLOCK);
        }
        let domain = tracepoint_srcu()?;
        domain.validate_update_context()?;
        let _update = self.callback_update.lock();
        let old = self.callbacks.load(Ordering::Acquire);
        if old.is_null() {
            return Err(SystemError::ENODEV);
        }
        // SAFETY: callback_update serializes publication and old remains owned by the slot.
        let mut next = unsafe { &*old }.try_clone()?;
        change(&mut next)?;
        let next = Box::try_new(next).map_err(|_| SystemError::ENOMEM)?;
        let old = self.callbacks.swap(Box::into_raw(next), Ordering::AcqRel);
        domain.synchronize_after_publication();
        // SAFETY: old was removed exactly once and the GP covers every prior reader.
        unsafe { drop(Box::from_raw(old)) };
        Ok(())
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
    pub fn register(
        &self,
        func: fn(),
        data: Box<dyn Any + Sync + Send>,
    ) -> Result<(), SystemError> {
        let ptr = func as usize;
        self.update_callbacks(|snapshot| {
            if snapshot
                .callbacks
                .iter()
                .any(|item| item.func as usize == ptr)
            {
                return Err(SystemError::EEXIST);
            }
            snapshot
                .callbacks
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
            snapshot.callbacks.push(TracePointFunc {
                func,
                data: data.into(),
            });
            Ok(())
        })
    }

    /// Unregister a callback function from the tracepoint
    pub fn unregister(&self, func: fn()) -> Result<(), SystemError> {
        let func_ptr = func as usize;
        self.update_callbacks(|snapshot| {
            let index = snapshot
                .callbacks
                .iter()
                .position(|item| item.func as usize == func_ptr)
                .ok_or(SystemError::ENOENT)?;
            snapshot.callbacks.remove(index);
            Ok(())
        })
    }

    /// Iterate over all registered callback functions
    pub fn callback_list(&self, f: &dyn Fn(&TracePointFunc)) {
        let Ok(domain) = tracepoint_srcu() else {
            return;
        };
        let _guard = domain.read_lock_notrace();
        let _depth = TracepointDepthGuard::enter();
        let snapshot = self.callbacks.load(Ordering::Acquire);
        if snapshot.is_null() {
            return;
        }
        // SAFETY: snapshot reclamation waits for this SRCU guard.
        for trace_func in unsafe { &*snapshot }.callbacks.iter() {
            f(trace_func);
        }
    }

    /// Register a raw callback function to the tracepoint
    ///
    /// This function will be called when default tracepoint fmt function is called.
    pub fn register_raw_callback(
        &self,
        callback_id: usize,
        callback: Arc<dyn TracePointCallBackFunc>,
    ) -> Result<(), SystemError> {
        self.update_callbacks(|snapshot| {
            match snapshot
                .raw_callbacks
                .binary_search_by_key(&callback_id, |entry| entry.0)
            {
                Ok(_) => Err(SystemError::EEXIST),
                Err(index) => {
                    snapshot
                        .raw_callbacks
                        .try_reserve(1)
                        .map_err(|_| SystemError::ENOMEM)?;
                    snapshot
                        .raw_callbacks
                        .insert(index, (callback_id, callback));
                    Ok(())
                }
            }
        })
    }

    /// Unregister a raw callback function from the tracepoint
    pub fn unregister_raw_callback(&self, callback_id: usize) -> Result<(), SystemError> {
        self.update_callbacks(|snapshot| {
            let index = snapshot
                .raw_callbacks
                .binary_search_by_key(&callback_id, |entry| entry.0)
                .map_err(|_| SystemError::ENOENT)?;
            snapshot.raw_callbacks.remove(index);
            Ok(())
        })
    }

    /// Atomically installs a group of raw callbacks and acquires one enable
    /// reference. Either the whole group becomes visible, or nothing changes.
    pub(crate) fn enable_raw_callbacks(
        &self,
        callbacks: &[(usize, Arc<dyn TracePointCallBackFunc>)],
    ) -> Result<(), SystemError> {
        let domain = tracepoint_srcu()?;
        if tracepoint_read_held_by_current() {
            return Err(SystemError::EDEADLK_OR_EDEADLOCK);
        }
        domain.validate_update_context()?;
        let _update = self.callback_update.lock();
        let old = self.callbacks.load(Ordering::Acquire);
        if old.is_null() {
            return Err(SystemError::ENODEV);
        }
        // SAFETY: publication is serialized and the slot still owns `old`.
        let mut next = unsafe { &*old }.try_clone()?;
        next.raw_callbacks
            .try_reserve(callbacks.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for (callback_id, callback) in callbacks {
            match next
                .raw_callbacks
                .binary_search_by_key(callback_id, |entry| entry.0)
            {
                Ok(_) => return Err(SystemError::EEXIST),
                Err(index) => next
                    .raw_callbacks
                    .insert(index, (*callback_id, callback.clone())),
            }
        }
        let next = Box::try_new(next).map_err(|_| SystemError::ENOMEM)?;
        self.acquire_enable()?;
        let old = self.callbacks.swap(Box::into_raw(next), Ordering::AcqRel);
        domain.synchronize_after_publication();
        // SAFETY: the old snapshot was removed once and the GP covers its readers.
        unsafe { drop(Box::from_raw(old)) };
        Ok(())
    }

    /// Atomically removes a group of raw callbacks and releases one enable
    /// reference. Preparation completes before either externally visible step.
    pub(crate) fn disable_raw_callbacks(
        &self,
        callbacks: &[(usize, Arc<dyn TracePointCallBackFunc>)],
    ) -> Result<(), SystemError> {
        let domain = tracepoint_srcu()?;
        if tracepoint_read_held_by_current() {
            return Err(SystemError::EDEADLK_OR_EDEADLOCK);
        }
        domain.validate_update_context()?;
        let _update = self.callback_update.lock();
        let old = self.callbacks.load(Ordering::Acquire);
        if old.is_null() {
            return Err(SystemError::ENODEV);
        }
        // SAFETY: publication is serialized and the slot still owns `old`.
        let mut next = unsafe { &*old }.try_clone()?;
        for (callback_id, _) in callbacks {
            let index = next
                .raw_callbacks
                .binary_search_by_key(callback_id, |entry| entry.0)
                .map_err(|_| SystemError::ENOENT)?;
            next.raw_callbacks.remove(index);
        }
        let next = Box::try_new(next).map_err(|_| SystemError::ENOMEM)?;
        self.release_enable()?;
        let old = self.callbacks.swap(Box::into_raw(next), Ordering::AcqRel);
        domain.synchronize_after_publication();
        // SAFETY: the old snapshot was removed once and the GP covers its readers.
        unsafe { drop(Box::from_raw(old)) };
        Ok(())
    }

    /// Snapshot all registered raw callbacks.
    ///
    /// Callback execution may allocate and run arbitrary BPF code, so it must
    /// not happen while the registry spin lock is held. `Arc` keeps callbacks
    /// alive when an owner unregisters after this snapshot was taken.
    pub fn for_each_raw_callback(&self, mut f: impl FnMut(&dyn TracePointCallBackFunc)) {
        let Ok(domain) = tracepoint_srcu() else {
            return;
        };
        if tracepoint_read_held_by_current() {
            self.for_each_raw_snapshot(&mut f);
            return;
        }
        let _guard = domain.read_lock_notrace();
        let _depth = TracepointDepthGuard::enter();
        self.for_each_raw_snapshot(&mut f);
    }

    fn for_each_raw_snapshot(&self, f: &mut impl FnMut(&dyn TracePointCallBackFunc)) {
        let snapshot = self.callbacks.load(Ordering::Acquire);
        if snapshot.is_null() {
            return;
        }
        // SAFETY: snapshot reclamation waits for this SRCU guard.
        for (_, callback) in unsafe { &*snapshot }.raw_callbacks.iter() {
            f(callback.as_ref());
        }
    }

    /// Acquire one active consumer reference.
    pub fn acquire_enable(&self) -> Result<(), SystemError> {
        crate::text_patch::validate_control_context().map_err(SystemError::from)?;
        let mut state = self.enable_state.lock();
        let users = state
            .users
            .checked_add(1)
            .expect("tracepoint enable reference overflow");
        if state.users == 0 {
            enable_maskable_key(self.key).map_err(SystemError::from)?;
        }
        state.users = users;
        Ok(())
    }

    /// Release one active consumer reference.
    pub fn release_enable(&self) -> Result<(), SystemError> {
        crate::text_patch::validate_control_context().map_err(SystemError::from)?;
        let mut state = self.enable_state.lock();
        let users = state
            .users
            .checked_sub(1)
            .expect("tracepoint enable reference underflow");
        if users == 0 {
            disable_maskable_key(self.key).map_err(SystemError::from)?;
        }
        state.users = users;
        Ok(())
    }

    /// Set the tracefs recording owner state. Repeated writes are idempotent.
    pub fn set_trace_pipe_enabled(&self, enabled: bool) -> Result<(), SystemError> {
        crate::text_patch::validate_control_context().map_err(SystemError::from)?;
        let mut state = self.enable_state.lock();
        if state.trace_pipe_enabled == enabled {
            return Ok(());
        }

        if enabled {
            let users = state
                .users
                .checked_add(1)
                .expect("tracepoint enable reference overflow");
            if state.users == 0 {
                enable_maskable_key(self.key).map_err(SystemError::from)?;
            }
            state.users = users;
            state.trace_pipe_enabled = true;
            self.trace_pipe_enabled.store(true, Ordering::Release);
        } else {
            let users = state
                .users
                .checked_sub(1)
                .expect("tracepoint enable reference underflow");
            if users == 0 {
                disable_maskable_key(self.key).map_err(SystemError::from)?;
            }
            state.users = users;
            state.trace_pipe_enabled = false;
            self.trace_pipe_enabled.store(false, Ordering::Release);
        }
        Ok(())
    }

    /// Whether tracefs owns an active recording reference.
    pub fn is_trace_pipe_enabled(&self) -> bool {
        self.trace_pipe_enabled.load(Ordering::Acquire)
    }
}
